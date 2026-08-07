// Device-room relay RPC client — dials a device's room on the edge as a
// `client` peer and speaks ControlRpc to the HOST engine over a virtual
// socket (crates/rpc/src/device_room.rs + edge/src/device-room.ts).
//
// Frame codec (binary WS messages): uleb128(headerLen) ‖ headerJSON ‖ payload.
// Header key order MUST be {"s","k","to","from"} (byte parity with both
// implementations); clients never set `to`/`from` — the DO stamps `from`.
// RPC payloads are ndjson ControlRpc frames: {id, method, params} out,
// {id, ok|err|item|done} back. Relay control frames (kind " relay" — leading
// space is part of the constant) signal host_offline/host_closed.
//
// Two call shapes, exactly the ones `comet-rpc`'s client has: `call` (unary,
// one `{ok}` or `{err}`) and `subscribe` (a stream of `{item}` until `{done}`
// or `{err}`). The stream half is what `WatchBoard` needs — until gh#114 this
// client only knew `{ok}`, so every `{item}` frame fell through to "unexpected
// reply" and no subscription on the engine was reachable from the phone at all.

import Foundation

enum RelayError: LocalizedError {
    case notConnected
    case hostOffline
    case rpc(String)
    case timeout

    var errorDescription: String? {
        switch self {
        case .notConnected: return "Not connected to the device"
        case .hostOffline: return "The device is offline"
        case .rpc(let message): return message
        case .timeout: return "The device didn't respond"
        }
    }
}

actor DeviceRelayClient {
    static let rpcKind = "rpc"
    static let relayKind = " relay"  // leading space is intentional

    private let deviceId: String
    private let config: AppConfig

    private var socket: URLSessionWebSocketTask?
    private var receiveTask: Task<Void, Never>?
    private var pingTask: Task<Void, Never>?
    private var nextId: UInt64 = 1
    private var pending: [UInt64: CheckedContinuation<Result<Data, RelayError>, Never>] = [:]
    /// Live subscriptions by request id — `{item}` frames feed these, `{done}`
    /// and `{err}` retire them, and a teardown fails every one at once so a
    /// standing watcher learns the link died instead of hanging forever.
    private var streams: [UInt64: AsyncThrowingStream<Data, Error>.Continuation] = [:]
    private var connected = false

    init(deviceId: String, config: AppConfig) {
        self.deviceId = deviceId
        self.config = config
    }

    // MARK: Lifecycle

    private func connect() async throws {
        if connected, socket != nil { return }
        guard let token = await config.currentToken() else { throw RelayError.notConnected }
        var components = URLComponents(url: config.edgeURL.appending(path: "device/\(deviceId)/ws"),
                                       resolvingAgainstBaseURL: false)!
        components.scheme = components.scheme == "http" ? "ws" : "wss"
        components.queryItems = [
            URLQueryItem(name: "role", value: "client"),
            // A reconnect is a new relay peer. Reusing a connId can briefly
            // leave two tagged sockets in the hibernating DO and route the
            // host's response to the stale predecessor.
            URLQueryItem(name: "connId", value: UUID().uuidString.lowercased()),
            URLQueryItem(name: "token", value: token),
        ]
        let task = URLSession.shared.webSocketTask(with: components.url!)
        socket = task
        task.resume()
        connected = true

        receiveTask = Task { [weak self] in
            while !Task.isCancelled {
                guard let self else { return }
                guard let sock = await self.socket else { return }
                do {
                    let message = try await sock.receive()
                    await self.handleInbound(message)
                } catch {
                    await self.teardown(error: .hostOffline)
                    return
                }
            }
        }
        pingTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: 30_000_000_000)
                guard let self else { return }
                await self.sendPing()
            }
        }
    }

    func close() {
        teardown(error: .notConnected)
    }

    private func teardown(error: RelayError) {
        receiveTask?.cancel()
        pingTask?.cancel()
        socket?.cancel(with: .goingAway, reason: nil)
        socket = nil
        connected = false
        let waiting = pending
        pending.removeAll()
        for (_, continuation) in waiting {
            continuation.resume(returning: .failure(error))
        }
        let watchers = streams
        streams.removeAll()
        for (_, continuation) in watchers {
            continuation.finish(throwing: error)
        }
    }

    private func sendPing() async {
        try? await socket?.send(.string("ping"))
    }

    // MARK: RPC

    /// One unary ControlRpc call to the host engine, 10s deadline (the engine
    /// itself caps folder listing at 6s).
    func call<Response: Decodable>(method: String, params: [String: Any]) async throws -> Response {
        for attempt in 0..<3 {
            do {
                return try await callOnce(method: method, params: params)
            } catch let error as RelayError {
                guard attempt < 2 else { throw error }
                switch error {
                case .hostOffline, .notConnected:
                    teardown(error: error)
                    try? await Task.sleep(nanoseconds: UInt64(attempt + 1) * 250_000_000)
                case .rpc, .timeout:
                    throw error
                }
            }
        }
        throw RelayError.notConnected
    }

    private func callOnce<Response: Decodable>(
        method: String,
        params: [String: Any]
    ) async throws -> Response {
        try await connect()
        let id = nextId
        nextId += 1
        // Always send a params object — the engine's serde rejects a missing
        // field even when every param is optional (ListFolders home listing).
        let frame: [String: Any] = ["id": id, "method": method, "params": params]
        let payload = try JSONSerialization.data(withJSONObject: frame)
        let data = Self.encodeFrame(header: #"{"s":"rpc","k":"rpc"}"#, payload: payload)

        // Install the waiter before sending. URLSession's async send may yield
        // long enough for a fast host reply to reach handleInbound; registering
        // afterward loses that reply and turns a successful call into a timeout.
        let result: Result<Data, RelayError> = await withCheckedContinuation { continuation in
            pending[id] = continuation
            Task {
                await self.send(data, for: id)
            }
            Task {
                try? await Task.sleep(nanoseconds: 10_000_000_000)
                self.timeoutCall(id: id)
            }
        }
        switch result {
        case .failure(let error): throw error
        case .success(let ok):
            return try JSONDecoder().decode(Response.self, from: ok)
        }
    }

    // MARK: Streaming RPC

    /// One streaming ControlRpc subscription (`WatchBoard` and friends): each
    /// `{item}` is yielded as raw JSON, `{done}` finishes the stream and `{err}`
    /// throws. No deadline — a watch is *supposed* to stay silent between
    /// changes; the link dying is what ends it, and the caller resubscribes.
    ///
    /// Cancelling the consuming task (or breaking out of the `for await`) sends
    /// `{id, cancel: true}`, which is how `comet-rpc`'s own client retires a
    /// stream: without it the host keeps producing into a socket nobody reads.
    func subscribe(method: String, params: [String: Any]) -> AsyncThrowingStream<Data, Error> {
        AsyncThrowingStream { continuation in
            Task { await self.beginSubscription(method: method, params: params,
                                                continuation: continuation) }
        }
    }

    private func beginSubscription(
        method: String,
        params: [String: Any],
        continuation: AsyncThrowingStream<Data, Error>.Continuation
    ) async {
        do {
            try await connect()
        } catch {
            continuation.finish(throwing: RelayError.notConnected)
            return
        }
        let id = nextId
        nextId += 1
        let frame: [String: Any] = ["id": id, "method": method, "params": params]
        guard let payload = try? JSONSerialization.data(withJSONObject: frame) else {
            continuation.finish(throwing: RelayError.rpc("could not encode \(method)"))
            return
        }
        streams[id] = continuation
        continuation.onTermination = { [weak self] termination in
            guard case .cancelled = termination else { return }
            Task { await self?.cancelSubscription(id: id) }
        }
        guard let socket else {
            streams.removeValue(forKey: id)
            continuation.finish(throwing: RelayError.notConnected)
            return
        }
        do {
            try await socket.send(.data(Self.encodeFrame(header: #"{"s":"rpc","k":"rpc"}"#,
                                                         payload: payload)))
        } catch {
            streams.removeValue(forKey: id)
            continuation.finish(throwing: RelayError.notConnected)
            teardown(error: .notConnected)
        }
    }

    /// Tell the host to stop producing for a stream the consumer walked away
    /// from. Best-effort: a dead link has already ended the stream anyway.
    private func cancelSubscription(id: UInt64) async {
        guard streams.removeValue(forKey: id) != nil, let socket else { return }
        let frame: [String: Any] = ["id": id, "cancel": true]
        guard let payload = try? JSONSerialization.data(withJSONObject: frame) else { return }
        try? await socket.send(.data(Self.encodeFrame(header: #"{"s":"rpc","k":"rpc"}"#,
                                                      payload: payload)))
    }

    private func send(_ data: Data, for id: UInt64) async {
        guard let socket else {
            failCall(id: id, error: .notConnected)
            return
        }
        do {
            try await socket.send(.data(data))
        } catch {
            failCall(id: id, error: .notConnected)
            teardown(error: .notConnected)
        }
    }

    private func failCall(id: UInt64, error: RelayError) {
        if let continuation = pending.removeValue(forKey: id) {
            continuation.resume(returning: .failure(error))
        }
    }

    private func timeoutCall(id: UInt64) {
        failCall(id: id, error: .timeout)
    }

    // MARK: Inbound

    private func handleInbound(_ message: URLSessionWebSocketTask.Message) {
        switch message {
        case .string:
            return  // "pong"
        case .data(let data):
            guard let (header, payload) = Self.decodeFrame(data) else { return }
            switch header.k {
            case Self.rpcKind:
                handleRpcPayload(payload)
            case Self.relayKind:
                // {"error":"host_offline"|"host_closed"|...} — link down.
                teardown(error: .hostOffline)
            default:
                return
            }
        @unknown default:
            return
        }
    }

    private func handleRpcPayload(_ payload: Data) {
        // ndjson: each line is one ServerFrame.
        guard let text = String(data: payload, encoding: .utf8) else { return }
        for line in text.split(separator: "\n") {
            guard let obj = try? JSONSerialization.jsonObject(with: Data(line.utf8)) as? [String: Any],
                  let id = (obj["id"] as? NSNumber)?.uint64Value else { continue }
            // A stream id and a call id are drawn from the same counter, so at
            // most one of these two owns the frame.
            if let watcher = streams[id] {
                handleStreamFrame(obj, id: id, watcher: watcher)
            } else if let continuation = pending.removeValue(forKey: id) {
                handleCallFrame(obj, continuation: continuation)
            }
        }
    }

    private func handleCallFrame(
        _ obj: [String: Any],
        continuation: CheckedContinuation<Result<Data, RelayError>, Never>
    ) {
        if let err = obj["err"] as? String {
            continuation.resume(returning: .failure(.rpc(err)))
        } else if obj.keys.contains("ok"),
                  let okData = try? JSONSerialization.data(withJSONObject: obj["ok"] ?? NSNull(),
                                                           options: .fragmentsAllowed) {
            continuation.resume(returning: .success(okData))
        } else {
            continuation.resume(returning: .failure(.rpc("unexpected reply")))
        }
    }

    private func handleStreamFrame(
        _ obj: [String: Any],
        id: UInt64,
        watcher: AsyncThrowingStream<Data, Error>.Continuation
    ) {
        if let err = obj["err"] as? String {
            streams.removeValue(forKey: id)
            watcher.finish(throwing: RelayError.rpc(err))
        } else if obj["done"] as? Bool == true {
            streams.removeValue(forKey: id)
            watcher.finish()
        } else if obj.keys.contains("item"),
                  let itemData = try? JSONSerialization.data(withJSONObject: obj["item"] ?? NSNull(),
                                                             options: .fragmentsAllowed) {
            watcher.yield(itemData)
        }
    }

    // MARK: Frame codec

    struct FrameHeader: Decodable {
        var s: String?
        var k: String?
        var to: String?
        var from: String?
    }

    static func encodeFrame(header: String, payload: Data) -> Data {
        let headerBytes = Data(header.utf8)
        var out = Data()
        var len = UInt64(headerBytes.count)
        repeat {
            var byte = UInt8(len & 0x7f)
            len >>= 7
            if len != 0 { byte |= 0x80 }
            out.append(byte)
        } while len != 0
        out.append(headerBytes)
        out.append(payload)
        return out
    }

    static func decodeFrame(_ data: Data) -> (FrameHeader, Data)? {
        var offset = 0
        var length: UInt64 = 0
        var shift: UInt64 = 0
        let bytes = [UInt8](data)
        while offset < bytes.count {
            let byte = bytes[offset]
            offset += 1
            length |= UInt64(byte & 0x7f) << shift
            if byte & 0x80 == 0 { break }
            shift += 7
            if shift > 28 { return nil }
        }
        guard offset + Int(length) <= bytes.count else { return nil }
        let headerData = Data(bytes[offset..<offset + Int(length)])
        guard let header = try? JSONDecoder().decode(FrameHeader.self, from: headerData) else { return nil }
        let payload = Data(bytes[(offset + Int(length))...])
        return (header, payload)
    }
}
