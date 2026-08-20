// Session doc mirror — transcript entries + the durable command queue for one
// chat (crates/doc/src/schema.rs). A viewer device never writes message
// entries; it appends command ledger entries (rule 1) and lets the host drain
// them. Optimistic echo: pending sends render locally under their client-minted
// message id until the host writes the real entry with the same id.

import Foundation
import Loro
import Observation

@MainActor
@Observable
final class SessionStore {
    let chatId: String
    /// The chat's host device — nudge target for cold-host command drains.
    var hostDeviceId: String?
    private(set) var entries: [MessageEntry] = []
    /// Bumped on every change to `entries` / `pendingSends`. The transcript's
    /// row builder memoizes on it, so a body re-eval that was triggered by
    /// something else (scrolling) costs O(1) instead of re-deriving every row.
    private(set) var revision: UInt64 = 0
    /// Whether this chat's transcript has already been revealed once.
    ///
    /// Lives on the store, not the view: the reveal gate is `@State`, so any
    /// re-creation of TranscriptView reset it to "hidden" and blanked an
    /// already-visible transcript until the settle loop finished. The store is
    /// cached per chat, so it outlives that churn.
    @ObservationIgnored var hasRevealed = false
    private(set) var connected = false
    /// Client-minted ids of sends the host hasn't materialized yet.
    private(set) var pendingSends: [(messageId: String, text: String, at: Int64)] = []
    private(set) var followups: [FollowupRow] = []
    private(set) var followupsPaused = false

    private let document = RoomDocument()
    var doc: LoroDoc { document.current() }
    private var room: RoomClient?
    private let config: AppConfig

    /// Demo mode: no room, entries driven externally.
    private let offline: Bool
    /// Demo hook: invoked instead of the command plane when offline.
    @ObservationIgnored var demoResponder: ((String) -> Void)?

    init(chatId: String, config: AppConfig, offline: Bool = false) {
        self.chatId = chatId
        self.config = config
        self.offline = offline
        AttachmentImageCache.shared.configure(config: config)
    }

    // MARK: Attachments — uploads go to the device that HOSTS this chat

    @ObservationIgnored private var hostRelay: (deviceId: String, client: DeviceRelayClient)?

    /// Chunked upload of one staged file to the chat's host device; returns the
    /// durable absolute path THERE, which is what the refs trailer carries.
    ///
    /// The phone hosts nothing, so there is no local fast path to fall into and
    /// no "current device" to default to: a chat on the box uploads to the box,
    /// a chat on a Mac uploads to that Mac, and a chat whose host we do not yet
    /// know refuses rather than guessing (gh#535).
    func uploadAttachment(name: String, data: Data) async throws -> String {
        guard let hostDeviceId, !hostDeviceId.isEmpty else { throw RelayError.hostOffline }
        let relay: DeviceRelayClient
        if let hostRelay, hostRelay.deviceId == hostDeviceId {
            relay = hostRelay.client
        } else {
            relay = DeviceRelayClient(deviceId: hostDeviceId, config: config)
            hostRelay = (hostDeviceId, relay)
        }
        return try await uploadAttachmentChunked(relay: relay, name: name, data: data)
    }

    /// Demo-mode injection point (also used by previews).
    func setEntries(_ new: [MessageEntry]) {
        entries = new
        revision &+= 1
    }

    @ObservationIgnored private var saver: DocSaver?

    func start() {
        guard room == nil, !offline else { return }
        // Local-first: last-synced transcript renders instantly (even when the
        // host device is offline); the join backfills incrementally from here.
        if DocDisk.load(into: doc, id: chatId) {
            project()
        }
        saver = DocSaver(docId: chatId, document: document)
        let client = RoomClient(roomId: chatId, document: document,
                                localDeviceId: config.deviceId) { [config, chatId] in
            await config.sessionSocketURL(chatId: chatId)
        } events: { [weak self] event in
            Task { @MainActor [weak self] in self?.handle(event) }
        }
        room = client
        document.observeLocalUpdates { [weak client, weak self] generation, update in
            guard let client else { return }
            let bytes = [UInt8](update)
            Task { await client.sendLocalUpdate(bytes, generation: generation) }
            Task { @MainActor [weak self] in self?.saver?.poke() }
        }
        Task { await client.start() }
        project()
    }

    /// Backgrounding hook: persist immediately.
    func flushToDisk() {
        saver?.flush()
    }

    func stop() {
        document.stopObservingLocalUpdates()
        saver?.flush()
        if let room {
            Task { await room.stop() }
        }
        room = nil
        connected = false
    }

    private func handle(_ event: RoomEvent) {
        switch event {
        case .connected:
            connected = true
            project()
        case .disconnected:
            connected = false
        case .remoteUpdate:
            project()
            saver?.poke()
        case .ephemeralUpdate:
            break
        }
    }

    // MARK: Projection

    /// In-flight guard + trailing re-run for the off-main projection below.
    @ObservationIgnored private var projecting = false
    @ObservationIgnored private var projectPending = false

    /// Re-derive `entries` from the doc, off the main thread.
    ///
    /// `getDeepValue()` materializes the WHOLE doc and the decode walks every
    /// message and every part, so this is O(transcript) — tens of ms on a big
    /// session, and it runs on every remote update. On the main actor that
    /// stalled the first frame of a cached session and janked streaming.
    /// Reading the doc from a background task is the access class the design
    /// already has: `RoomClient` is a non-main actor that imports into this
    /// same doc, so it is concurrently read/written today regardless.
    ///
    /// Overlapping calls coalesce to a single trailing re-run — a streaming
    /// burst must not queue one whole-doc projection per token.
    private func project() {
        guard !projecting else {
            projectPending = true
            return
        }
        projecting = true
        let doc = self.doc
        Task { @MainActor [weak self] in
            let decoded = await Task.detached(priority: .userInitiated) {
                Self.decodeSession(from: doc)
            }.value
            guard let self else { return }
            self.projecting = false
            if let decoded {
                self.apply(decoded.entries)
                self.followups = decoded.followups
                self.followupsPaused = decoded.paused
            }
            if self.projectPending {
                self.projectPending = false
                self.project()
            }
        }
    }

    private func apply(_ decoded: [MessageEntry]) {
        entries = decoded
        // Drop echoes the host has materialized.
        let ids = Set(entries.map(\.id))
        pendingSends.removeAll { ids.contains($0.messageId) }
        revision &+= 1
    }

    /// Whole-doc decode. `nil` means the doc has no map root yet — leave the
    /// previous projection standing rather than blanking a live transcript.
    nonisolated static func decodeEntries(from doc: LoroDoc) -> [MessageEntry]? {
        guard let root = doc.getDeepValue().mapValue else { return nil }
        let raw = (root["messages"]?.listValue ?? []).compactMap(entryFrom)
        return joinContinuations(raw)
    }

    private struct DecodedSession {
        var entries: [MessageEntry]
        var followups: [FollowupRow]
        var paused: Bool
    }

    nonisolated private static func decodeSession(from doc: LoroDoc) -> DecodedSession? {
        guard let root = doc.getDeepValue().mapValue else { return nil }
        let entries = joinContinuations(
            (root["messages"]?.listValue ?? []).compactMap(entryFrom)
        )
        var rows: [FollowupRow] = []
        var paused = false
        for value in root["commands"]?.listValue ?? [] {
            guard let command = value.mapValue,
                  let payload = command["payload"]?.mapValue,
                  let kind = payload["kind"]?.stringValue else { continue }
            switch kind {
            case "queue":
                guard command["status"]?.stringValue == "pending",
                      let id = command["id"]?.stringValue,
                      let prompt = payload["prompt"]?.stringValue,
                      let messageId = payload["messageId"]?.stringValue else { continue }
                let context = decodeContext(command["context"])
                rows.append(FollowupRow(id: id, prompt: prompt, context: context,
                                        messageId: messageId, edited: false))
            case "interrupt":
                if !rows.isEmpty { paused = true }
            case "queueControl":
                guard let op = payload["op"]?.mapValue,
                      let tag = op["op"]?.stringValue else { continue }
                let target = op["target"]?.stringValue
                switch tag {
                case "edit":
                    if let target, let ix = rows.firstIndex(where: { $0.id == target }) {
                        rows[ix].prompt = op["prompt"]?.stringValue ?? rows[ix].prompt
                        rows[ix].context = decodeContext(op["context"])
                        rows[ix].edited = true
                    }
                case "move":
                    if let target, let from = rows.firstIndex(where: { $0.id == target }) {
                        let row = rows.remove(at: from)
                        if let after = op["after"]?.stringValue,
                           let anchor = rows.firstIndex(where: { $0.id == after }) {
                            rows.insert(row, at: anchor + 1)
                        } else if op["after"] == nil {
                            rows.insert(row, at: 0)
                        } else {
                            rows.append(row)
                        }
                    }
                case "remove": rows.removeAll { $0.id == target }
                case "clear": rows.removeAll()
                case "pause": paused = true
                case "resume": paused = false
                default: break // runNext changes delivery, not visible order
                }
            default: break
            }
        }
        return DecodedSession(entries: entries, followups: rows, paused: paused)
    }

    nonisolated private static func decodeContext(_ value: LoroValue?) -> [ContextRef] {
        guard let list = value?.listValue,
              let data = try? JSONSerialization.data(withJSONObject: list.map(\.jsonObject)),
              let decoded = try? JSONDecoder().decode([ContextRef].self, from: data) else {
            return []
        }
        return decoded
    }

    nonisolated private static func entryFrom(_ value: LoroValue) -> MessageEntry? {
        guard let m = value.mapValue,
              let id = m["id"]?.stringValue,
              let roleStr = m["role"]?.stringValue,
              let role = MessageRole(rawValue: roleStr) else { return nil }
        let parts = (m["parts"]?.listValue ?? []).compactMap(partFrom)
        return MessageEntry(id: id, role: role, parts: parts,
                            createdAt: m["createdAt"]?.i64Value ?? 0,
                            deviceId: m["deviceId"]?.stringValue ?? "",
                            status: m["status"]?.stringValue.flatMap(MessageStatus.init(rawValue:)),
                            continuationOf: m["continuationOf"]?.stringValue)
    }

    nonisolated private static func partFrom(_ value: LoroValue) -> MessagePart? {
        guard let m = value.mapValue,
              let id = m["id"]?.stringValue,
              let kind = m["kind"]?.stringValue else { return nil }
        switch kind {
        case "text":
            return .text(id: id, text: m["text"]?.stringValue ?? "")
        case "tool":
            guard let callMap = m["call"]?.mapValue else { return nil }
            let tag = callMap["kind"]?.stringValue ?? "unknown"
            var fields: [String: AnyHashable] = [:]
            for (k, v) in callMap where k != "kind" {
                if let s = v.stringValue { fields[k] = s }
                else if let b = v.boolValue { fields[k] = b }
                else if let i = v.i64Value { fields[k] = i }
                else if let list = v.listValue {
                    // ApplyPatch changes / Todo items — keep a JSON echo.
                    fields[k] = list.map { "\($0.jsonObject)" }
                }
            }
            // isError presence IS the resolution marker (schema.rs:96).
            let isError = m["isError"]?.boolValue
            return .tool(id: id, call: RenderToolCall(tag: tag, fields: fields),
                         isError: isError ?? false, resolved: isError != nil)
        case "input":
            var questions: [UserInputQuestion] = []
            if let list = m["questions"]?.listValue,
               let data = try? JSONSerialization.data(withJSONObject: list.map(\.jsonObject)),
               let decoded = try? JSONDecoder().decode([UserInputQuestion].self, from: data) {
                questions = decoded
            }
            return .input(id: id, requestId: id, questions: questions,
                          resolved: m["resolved"]?.boolValue ?? false)
        case "error":
            return .error(id: id, message: m["message"]?.stringValue ?? "")
        default:
            return nil
        }
    }

    /// schema.rs join_continuation_entries: concatenate continuation parts onto
    /// the root in list order; orphans surface standalone.
    nonisolated static func joinContinuations(_ raw: [MessageEntry]) -> [MessageEntry] {
        var roots: [MessageEntry] = []
        var index: [String: Int] = [:]
        for entry in raw {
            if let rootId = entry.continuationOf, let ix = index[rootId] {
                roots[ix].parts.append(contentsOf: entry.parts)
            } else {
                index[entry.id] = roots.count
                roots.append(entry)
            }
        }
        return roots
    }

    // MARK: Derived

    var lastEntryId: String? { entries.last?.id }

    var liveEntry: MessageEntry? {
        entries.last(where: { $0.status == .streaming })
    }

    /// The unresolved input request to surface in the question panel.
    var openInputRequest: (entryId: String, requestId: String, questions: [UserInputQuestion])? {
        for entry in entries.reversed() {
            for part in entry.parts.reversed() {
                // An empty question list can't be answered, so it must not take
                // the composer's place — leaving the user with no way to type.
                if case .input(_, let requestId, let questions, let resolved) = part,
                   !resolved, !questions.isEmpty {
                    return (entry.id, requestId, questions)
                }
            }
        }
        return nil
    }

    // MARK: Command plane (ledger rule 1: append-only, own entries only)

    func sendRun(prompt: String, chat: Chat, context: [ContextRef] = [],
                 attachments: [String] = []) {
        if offline {
            demoResponder?(prompt)
            return
        }
        let messageId = UUID().uuidString.lowercased()
        let request = RunRequest(prompt: prompt,
                                 model: chat.config?.model,
                                 reasoning: chat.config?.reasoning,
                                 cwd: chat.cwd ?? "",
                                 sandbox: chat.config?.sandbox ?? "workspace-write",
                                 attachments: attachments)
        queueCommand(kind: "run", payload: [
            "kind": "run",
            "request": encodableJSON(request),
            "messageId": messageId,
        ], context: context)
        pendingSends.append((messageId, prompt, nowMs()))
        revision &+= 1
    }

    func sendSteer(prompt: String, context: [ContextRef] = []) {
        if offline {
            demoResponder?(prompt)
            return
        }
        let messageId = UUID().uuidString.lowercased()
        queueCommand(kind: "steer", payload: [
            "kind": "steer",
            "prompt": prompt,
            "messageId": messageId,
        ], context: context)
        pendingSends.append((messageId, prompt, nowMs()))
        revision &+= 1
    }

    func sendInterrupt() {
        queueCommand(kind: "interrupt", payload: ["kind": "interrupt"])
    }

    func respondInput(requestId: String, answers: [UserInputAnswer]) {
        queueCommand(kind: "respondInput", payload: [
            "kind": "respondInput",
            "requestId": requestId,
            "answers": answers.map(encodableJSON),
        ])
    }

    @discardableResult
    func queueFollowup(prompt: String, context: [ContextRef] = [],
                       attachments: [String] = []) -> Bool {
        queueCommand(kind: "queue", payload: [
            "kind": "queue",
            "prompt": prompt,
            "messageId": UUID().uuidString.lowercased(),
            // The host stamps these onto the RunRequest when this row's turn
            // comes — a follow-up's files are its own, never the last turn's.
            "attachments": attachments,
        ], context: context, expires: false)
    }

    @discardableResult
    func editFollowup(id: String, prompt: String, context: [ContextRef] = []) -> Bool {
        queueControl(["op": "edit", "target": id, "prompt": prompt,
                      "context": context.map(encodableJSON)])
    }

    @discardableResult
    func moveFollowup(id: String, after: String?) -> Bool {
        var op: [String: Any] = ["op": "move", "target": id]
        if let after { op["after"] = after }
        return queueControl(op)
    }

    @discardableResult
    func removeFollowup(id: String) -> Bool { queueControl(["op": "remove", "target": id]) }
    @discardableResult
    func runNext(id: String) -> Bool { queueControl(["op": "runNext", "target": id]) }
    @discardableResult
    func setFollowupsPaused(_ paused: Bool) -> Bool {
        queueControl(["op": paused ? "pause" : "resume"])
    }

    @discardableResult
    private func queueControl(_ op: [String: Any]) -> Bool {
        queueCommand(kind: "queueControl", payload: ["kind": "queueControl", "op": op],
                     expires: false)
    }

    /// schema.rs queue_command, field for field.
    @discardableResult
    private func queueCommand(kind: String, payload: [String: Any],
                              context: [ContextRef] = [], expires: Bool = true) -> Bool {
        let queued = document.withCurrent { doc -> Bool in
            let commands = doc.getList(id: "commands")
            do {
                let map = try commands.pushContainer(child: LoroMap())
                try map.insert(key: "id", v: UUID().uuidString.lowercased())
                try map.insert(key: "kind", v: kind)
                try map.insert(key: "payload", v: LoroValue.fromJSON(payload))
                try map.insert(key: "issuedBy", v: config.deviceId)
                try map.insert(key: "issuedAt", v: nowMs())
                if let turnId = lastEntryId {
                    try map.insert(key: "basedOn", v: LoroValue.map(value: [
                        "turnId": .string(value: turnId),
                        "frontier": .null,
                    ]))
                }
                if expires { try map.insert(key: "expiresAt", v: nowMs() + commandDefaultTtlMs) }
                if !context.isEmpty {
                    try map.insert(key: "context", v: LoroValue.fromJSON(context.map(encodableJSON)))
                }
                try map.insert(key: "status", v: "pending")
                doc.commit()
                return true
            } catch {
                roomLog.error("session \(self.chatId, privacy: .public): failed to queue \(kind, privacy: .public) command")
                return false
            }
        }
        if queued { nudgeHost() }
        return queued
    }

    /// Durable-nudge the host device so a cold host opens the doc and drains
    /// (doc_host.rs nudge_remote_host). Fire-and-forget; the command is
    /// durable in the doc regardless.
    private func nudgeHost() {
        guard let hostDeviceId else { return }
        Task { [config, chatId] in
            await config.nudge(deviceId: hostDeviceId, chatId: chatId)
        }
    }
}

private func encodableJSON<T: Encodable>(_ value: T) -> Any {
    guard let data = try? JSONEncoder().encode(value),
          let obj = try? JSONSerialization.jsonObject(with: data) else { return [:] }
    return obj
}
