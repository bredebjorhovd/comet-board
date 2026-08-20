// Loro room client — a Swift port of crates/sync/src/room.rs.
//
// One client per room (workspace doc or session doc), one WebSocket carrying
// two sub-rooms: the `%LOR` doc room and the `%EPH` presence room. The client
// joins with its local oplog VV, imports the server's backfill, resubmits
// anything the server lacks (covers unacked updates across reconnects), and
// relays local commits as DocUpdate batches until acked.

import Foundation
import Loro
import os

enum RoomEvent {
    case connected
    case disconnected
    case remoteUpdate
    case ephemeralUpdate
    /// The room's CONTENT state changed — converged, pending, recovering or
    /// blocked (gh#483). Emitted independently of `.connected` on purpose: the
    /// incident is a room that is connected and never converges, and a surface
    /// that infers one from the other cannot show it.
    case convergenceChanged
}

/// Sync must never fail silently (2026-07-31: a send that never left the
/// device was indistinguishable from a working one — `try?` all the way
/// down). Visible in Console.app / `log stream` under this subsystem.
let roomLog = Logger(subsystem: "dev.cometnative.Comet", category: "sync")

/// One swappable document reference shared by the room, its store, local
/// update forwarding, and disk persistence. A shallow snapshot cannot repair a
/// stale LoroDoc in place, so recovery must change the object every consumer
/// reads rather than only changing RoomClient's private pointer.
final class RoomDocument: @unchecked Sendable {
    private let lock = NSLock()
    private var doc: LoroDoc
    private var generation: UInt64 = 0
    private var localUpdateHandler: ((UInt64, Data) -> Void)?
    private var localSubscription: Subscription?

    init(_ doc: LoroDoc = LoroDoc()) {
        self.doc = doc
    }

    func current() -> LoroDoc {
        lock.lock()
        defer { lock.unlock() }
        return doc
    }

    func currentGeneration() -> UInt64 {
        lock.lock()
        defer { lock.unlock() }
        return generation
    }

    /// Hold the owner gate for the whole operation. Device-command creation
    /// uses this instead of retaining `current()` across a reseed, so either
    /// the command commits before the final replay or it lands directly on the
    /// replacement — never on an orphaned old doc.
    func withCurrent<T>(_ operation: (LoroDoc) throws -> T) rethrows -> T {
        lock.lock()
        defer { lock.unlock() }
        return try operation(doc)
    }

    func observeLocalUpdates(_ handler: @escaping (UInt64, Data) -> Void) {
        lock.lock()
        defer { lock.unlock() }
        localUpdateHandler = handler
        let observedGeneration = generation
        localSubscription = doc.subscribeLocalUpdate { update in
            handler(observedGeneration, update)
        }
    }

    func stopObservingLocalUpdates() {
        lock.lock()
        defer { lock.unlock() }
        localSubscription = nil
        localUpdateHandler = nil
    }

    /// Run `recover` against (stale, replacement) and swap the owner under the
    /// same gate used by command creation — one atomic operation, so a command
    /// either commits on the old owner and is replayed by `recover`, or lands
    /// directly on the replacement.
    ///
    /// `recover` is `ConvergenceRecovery.recover` (gh#483): it quarantines the
    /// stale document and replays EVERY locally committed semantic entry onto
    /// the replacement, not merely this device's unresolved command intent.
    /// Returning nil refuses the swap, which leaves the stale — unmergeable but
    /// complete — document in place.
    func replace(with replacement: LoroDoc,
                 recover: (LoroDoc, LoroDoc) -> ReplayReport?) -> ReplayReport? {
        lock.lock()
        defer { lock.unlock() }
        guard let report = recover(doc, replacement) else { return nil }
        generation &+= 1
        let replacementGeneration = generation
        let replacementSubscription = localUpdateHandler.map { handler in
            replacement.subscribeLocalUpdate { update in
                handler(replacementGeneration, update)
            }
        }
        doc = replacement
        localSubscription = replacementSubscription
        return report
    }
}

actor RoomClient {
    // Constants mirrored from room.rs.
    static let fragmentBytes = 200_000
    static let pingIntervalNs: UInt64 = 30_000_000_000
    static let silenceLeaseNs: UInt64 = 45_000_000_000
    // The redial ladder — its rungs, the lifetime that earns a reset, and the
    // jitter on every wait — is `ReconnectBackoff` (gh#405). It is a value
    // type so the decision can be asserted without a socket, which is why it
    // is not three constants in this list.
    static let maxInvalidRejoins = 3
    static let maxFragmentCount: UInt64 = 4096
    static let maxReassembledBytes = 64 * 1024 * 1024
    // Room-level liveness (room.rs, 2026-07-30 incident): the silence lease
    // above is TRANSPORT-only — the CF runtime auto-answers our text pings
    // without ever waking the DO, so a wedged room looks healthy forever if
    // pongs are all we judge by. Room liveness is judged on %LOR frames plus
    // a hard join-answer deadline; %EPH presence traffic deliberately does
    // not count (the edge's eph path never touches the doc machinery).
    static let joinDeadlineNs: UInt64 = 15_000_000_000
    static let roomProbeAfterNs: UInt64 = 900_000_000_000
    static let roomProbeMaxNs: UInt64 = 4 * 3_600_000_000_000
    static let probeReplyGraceNs: UInt64 = 30_000_000_000
    static let livenessTickNs: UInt64 = 5_000_000_000
    /// Liveness ticks per convergence recomputation — 3 × 5s = the 15s
    /// `CONVERGENCE_POLL` the desktop uses.
    static let convergencePollTicks = 3

    let roomId: String
    let document: RoomDocument
    private let localDeviceId: String?
    private var doc: LoroDoc { document.current() }
    let eph: EphemeralStore
    private let urlProvider: @Sendable () async -> URL?
    private let events: @Sendable (RoomEvent) -> Void

    private var socket: URLSessionWebSocketTask?
    private var receiveTask: Task<Void, Never>?
    private var pingTask: Task<Void, Never>?
    private var livenessTask: Task<Void, Never>?
    private var pending: [BatchId: [[UInt8]]] = [:]
    private var fragments: [BatchId: FragmentBuffer] = [:]
    private var joinedLor = false
    /// Instant the FIRST `%LOR` join of THIS session was answered — a rejoin or
    /// a liveness probe does not restart it (room.rs `Session::joined_at`). The
    /// session's lifetime as a joined room is what earns a backoff reset; see
    /// `ReconnectBackoff.healthySessionNs` for why the join answer alone does
    /// not (gh#405).
    private var joinedAt: DispatchTime?
    private var invalidRejoins = 0
    private var fullResyncRequested = false
    private var serverVv: VersionVector?
    private var backoff = ReconnectBackoff()
    private var lastInbound = DispatchTime.now()
    private var closed = false
    private var protocolParked = false
    private var generation = 0
    // Room-liveness state (room.rs Session::{join_sent_at, join_is_probe,
    // last_lor_rx}): the instant of the last %LOR JoinRequest still awaiting
    // JoinResponseOk, whether that join is a liveness/recovery probe on an
    // established session (its answer must not replay join side effects), and
    // the last
    // inbound %LOR frame — the clock feeding both the deadline and the probe.
    /// Convergence recovery for this room's document (gh#483): the durable
    /// semantic outbox, the quarantine, the replay, and the truthful content
    /// state. Nil only where a caller has no persistence to key it to.
    private let convergence: ConvergenceRecovery?
    /// Everything this session has PROOF the edge holds: the version it
    /// advertised on join, merged with the end version of every batch it
    /// acked. Only this retires outbox rows — never the fact that a frame was
    /// sent.
    private var ackedVv = VersionVector()
    private var convergenceState: ConvergenceState = .converged
    private var convergencePollTicks = 0
    private var unackedSince: DispatchTime?
    private var stallReported = false
    /// Set when the edge refuses this device's content and recovery cannot
    /// repair it: the room is live and the document is local-only.
    private var blockedReason: String?
    private var joinSentAt: DispatchTime?
    private var joinIsProbe = false
    private var lastLorRx = DispatchTime.now()
    private var probeIntervalNs = RoomClient.roomProbeAfterNs
    private var lastProbeAt: DispatchTime?

    private struct FragmentBuffer {
        var crdt: CrdtType
        var parts: [[UInt8]?]
        var received: Int
        var totalSize: Int
    }

    init(roomId: String,
         document: RoomDocument,
         localDeviceId: String? = nil,
         ephTimeoutMs: Int64 = 30_000,
         convergence: ConvergenceRecovery? = nil,
         urlProvider: @escaping @Sendable () async -> URL?,
         events: @escaping @Sendable (RoomEvent) -> Void) {
        self.roomId = roomId
        self.document = document
        self.localDeviceId = localDeviceId
        self.eph = EphemeralStore(timeout: ephTimeoutMs)
        self.convergence = convergence
        self.urlProvider = urlProvider
        self.events = events
    }

    /// What this room can honestly say about its CONTENT right now.
    ///
    /// Strictly independent of the socket: a room can be joined, ponging and
    /// presence-live while holding entries the edge has never taken — that is
    /// gh#483 — and it reads `.pending` here while everything else reads
    /// healthy. Anything rendering "synced" must read this.
    func contentState() -> ConvergenceState { convergenceState }

    /// Recompute and publish the content state (gh#483 §4/§7). Derived from
    /// three durable facts and nothing about the socket: unacknowledged
    /// content, an open recovery, and whether the edge has refused us.
    private func publishConvergence() {
        guard let convergence else { return }
        let unacked = convergence.pending
        if unacked == 0 {
            unackedSince = nil
            stallReported = false
        } else if unackedSince == nil {
            unackedSince = .now()
        }
        let stalled = unackedSince.map {
            DispatchTime.now().uptimeNanoseconds &- $0.uptimeNanoseconds
                >= convergenceUnacknowledgedAlertNs
        } ?? false
        let state: ConvergenceState
        if let blockedReason {
            state = .blockedLocalOnly(unacked: unacked, reason: blockedReason)
        } else if let record = convergence.openRecovery, record.phase < .acknowledged {
            state = .recovering(phase: record.phase, unacked: unacked)
        } else if unacked > 0 {
            state = .pending(unacked: unacked, stalled: stalled)
        } else {
            state = .converged
        }
        // The diagnostic gh#483 §7 asks for: JOINED, and uploads unacknowledged
        // past the threshold. Once per session, at fault level, naming the
        // count — the state a view renders is this same value, so the log and
        // the UI cannot disagree.
        if stalled, !stallReported, joinedLor {
            stallReported = true
            roomLog.fault("room \(self.roomId, privacy: .public): joined, but \(unacked) local entr(ies) have been unacknowledged past the threshold — transport liveness is NOT convergence")
        }
        guard state != convergenceState else { return }
        convergenceState = state
        events(.convergenceChanged)
    }

    /// Fold new proof of what the edge holds into `ackedVv` and retire the
    /// outbox rows it covers.
    private func noteAcknowledged(_ version: VersionVector) {
        ackedVv.merge(other: version)
        _ = convergence?.acknowledge(ackedVv)
        publishConvergence()
    }

    /// The version an acked batch carried, read from the blob itself rather
    /// than from what we believe we exported: a batch may predate a reseed, and
    /// only the bytes know what is in it.
    private func batchVersion(_ updates: [[UInt8]]) -> VersionVector? {
        var merged = VersionVector()
        var any = false
        for update in updates {
            guard let meta = try? decodeImportBlobMeta(bytes: Data(update), checkChecksum: false)
            else { return nil }
            merged.merge(other: meta.partialEndVv)
            any = true
        }
        return any ? merged : nil
    }

    // MARK: Lifecycle

    func start() {
        closed = false
        protocolParked = false
        // Crash resume, before the first dial: a recovery interrupted between
        // quarantine and acknowledgement finishes here, replaying the
        // quarantined content into whatever document this client holds.
        // Idempotent by stable id, so a resume with nothing to do costs one
        // file lookup (gh#483 §6).
        if let convergence, let report = convergence.resume(into: doc), !report.isEmpty {
            roomLog.error("room \(self.roomId, privacy: .public): replayed an interrupted recovery's quarantined content at open (\(report.total) entr(ies))")
        }
        publishConvergence()
        connect()
    }

    func stop() {
        closed = true
        generation += 1
        receiveTask?.cancel()
        pingTask?.cancel()
        livenessTask?.cancel()
        socket?.cancel(with: .goingAway, reason: nil)
        socket = nil
        joinedLor = false
        joinedAt = nil
    }

    private func connect() {
        guard !closed, !protocolParked else { return }
        generation += 1
        let gen = generation
        joinedLor = false
        joinedAt = nil
        fullResyncRequested = false
        serverVv = nil
        fragments.removeAll()
        joinSentAt = nil
        joinIsProbe = false
        lastLorRx = .now()
        probeIntervalNs = RoomClient.roomProbeAfterNs
        lastProbeAt = nil

        Task {
            guard let url = await urlProvider() else {
                // No URL = no token (refresh failed or signed out) — the
                // single most confusing silent failure: everything cached
                // renders, nothing syncs.
                roomLog.error("room \(self.roomId, privacy: .public): no socket URL (token unavailable); backing off")
                await self.scheduleReconnect(gen: gen)
                return
            }
            await self.openSocket(url: url, gen: gen)
        }
    }

    private func openSocket(url: URL, gen: Int) {
        guard gen == generation, !closed, !protocolParked else { return }
        let task = URLSession.shared.webSocketTask(with: url)
        socket = task
        task.resume()
        lastInbound = .now()

        receiveTask = Task { [weak self] in
            while !Task.isCancelled {
                guard let self else { return }
                guard let sock = await self.currentSocket(gen: gen) else { return }
                do {
                    let message = try await sock.receive()
                    await self.handleInbound(message, gen: gen)
                } catch {
                    await self.onSocketError(gen: gen)
                    return
                }
            }
        }

        pingTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: RoomClient.pingIntervalNs)
                guard let self else { return }
                await self.pingTick(gen: gen)
            }
        }

        livenessTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: RoomClient.livenessTickNs)
                guard let self else { return }
                await self.livenessTick(gen: gen)
            }
        }

        // Join the doc room with our local VV (empty VV asks for a snapshot).
        Task { await self.sendJoinLoro(version: self.localVersionBytes()) }
    }

    private func currentSocket(gen: Int) -> URLSessionWebSocketTask? {
        gen == generation ? socket : nil
    }

    private func onSocketError(gen: Int) {
        guard gen == generation, !closed, !protocolParked else { return }
        events(.disconnected)
        scheduleReconnect(gen: gen)
    }

    private func scheduleReconnect(gen: Int) {
        guard gen == generation, !closed, !protocolParked else { return }
        socket?.cancel(with: .abnormalClosure, reason: nil)
        socket = nil
        receiveTask?.cancel()
        pingTask?.cancel()
        livenessTask?.cancel()
        // A session that did real work earns a fresh ladder; one that only got
        // a join answer before dying does not, and neither does one that never
        // reached a socket at all (no token). Before gh#405 the reset fired on
        // the join answer itself, so a room whose DO accepted the join and then
        // died redialed at 250ms with no ceiling — see
        // `ReconnectBackoff.healthySessionNs`.
        let healthy = ReconnectBackoff.isHealthy(joinedAt: joinedAt)
        let delay = backoff.nextDelayMs(healthy: healthy)
        // The decision, in the log, because "why is this room quiet" is
        // answered by `healthy` and nothing else: a false there is a room that
        // is climbing the ladder rather than stuck.
        roomLog.warning("room \(self.roomId, privacy: .public): redialing in \(delay)ms (joined=\(self.joinedLor), healthy=\(healthy), rung=\(self.backoff.rungMs)ms)")
        Task {
            try? await Task.sleep(nanoseconds: UInt64(delay) * 1_000_000)
            await self.connect()
        }
    }

    private func pingTick(gen: Int) async {
        guard gen == generation, let socket else { return }
        let silence = DispatchTime.now().uptimeNanoseconds - lastInbound.uptimeNanoseconds
        if silence > RoomClient.silenceLeaseNs {
            roomLog.warning("room \(self.roomId, privacy: .public): socket silent past lease; treating as dead")
            onSocketError(gen: gen)
            return
        }
        try? await socket.send(.string("ping"))
    }

    /// Two-tier room liveness (room.rs run_session): an in-flight %LOR
    /// JoinRequest has a hard answer deadline; otherwise a long-quiet room
    /// gets an idempotent rejoin probe. The deadline runs from the LATER of
    /// join-sent and last %LOR frame, so a join queued behind a draining
    /// backfill that keeps producing %LOR acks is never killed mid-push. A
    /// hibernating-but-healthy DO is simply woken by the probe and answers —
    /// hibernation is NOT death, which is why probes run in minutes while
    /// the transport lease runs in seconds.
    private func livenessTick(gen: Int) async {
        guard gen == generation, socket != nil, !closed else { return }
        // The document's owner writes the outbox, not this actor, so "is this
        // room converged?" has to be ASKED rather than waited for (gh#483
        // §4/§7; `CONVERGENCE_POLL` in room.rs). Every third liveness tick =
        // 15s, and nothing here touches the socket.
        convergencePollTicks += 1
        if convergencePollTicks % RoomClient.convergencePollTicks == 0 {
            publishConvergence()
        }
        let now = DispatchTime.now().uptimeNanoseconds
        if let sent = joinSentAt {
            let base = max(sent.uptimeNanoseconds, lastLorRx.uptimeNanoseconds)
            if now - base > RoomClient.joinDeadlineNs {
                // The 2026-07-30 hang: a room that accepted the socket but
                // never answered the join. Redial via the backoff loop — one
                // fresh dial re-instantiates a wedged DO.
                roomLog.warning("room \(self.roomId, privacy: .public): no JoinResponseOk within deadline; room presumed wedged, redialing")
                onSocketError(gen: gen)
            }
            return
        }
        if now - lastLorRx.uptimeNanoseconds > probeIntervalNs {
            // Quiet room: rejoin as a liveness probe. Consecutive quiet
            // probes back off so a dormant chat costs a handful of DO wakes
            // a day; any organic %LOR traffic resets the cadence. Probe
            // state is armed BEFORE the send suspends — the actor is
            // reentrant across the await, so the answer could otherwise be
            // handled while joinIsProbe is still false and replay join side
            // effects.
            joinSentAt = .now()
            joinIsProbe = true
            lastProbeAt = .now()
            probeIntervalNs = min(probeIntervalNs * 2, RoomClient.roomProbeMaxNs)
            await send(.joinRequest(crdt: .loro, roomId: roomId, auth: [],
                                    version: localVersionBytes()))
        }
    }

    // MARK: Inbound

    private func handleInbound(_ message: URLSessionWebSocketTask.Message, gen: Int) async {
        guard gen == generation else { return }
        lastInbound = .now()
        switch message {
        case .string:
            return  // "pong" — lease already refreshed
        case .data(let data):
            guard let frame = LoroWire.decode(data) else { return }
            await handleFrame(frame, gen: gen)
        @unknown default:
            return
        }
    }

    private func handleFrame(_ frame: ProtocolMessage, gen: Int) async {
        // Advance the room-liveness clock for %LOR frames only: %EPH presence
        // keeps flowing from a doc-wedged DO, so letting it count silenced
        // the probe on exactly the room that wedged (room.rs, round-2
        // adversarial finding). Frames arriving inside a probe's own reply
        // window must not reset the probe backoff, or every probe would
        // reset its own decay.
        if crdtOf(frame) == .loro {
            lastLorRx = .now()
            if let at = lastProbeAt,
               DispatchTime.now().uptimeNanoseconds - at.uptimeNanoseconds
                   <= RoomClient.probeReplyGraceNs {
                // Probe's own reply — leave the backoff decaying.
            } else {
                probeIntervalNs = RoomClient.roomProbeAfterNs
            }
        }
        switch frame {
        case .joinResponseOk(let crdt, _, _, let version, _):
            await onJoinOk(crdt: crdt, version: version)

        case .joinError(let crdt, _, let code, let message):
            roomLog.error("room \(self.roomId, privacy: .public): join error \(String(describing: code), privacy: .public): \(message, privacy: .public)")
            if crdt == .loro {
                if code == .versionUnknown {
                    // Server can't diff from our VV — full snapshot backfill.
                    await requestFullResync()
                } else {
                    // AuthFailed / AppError: back off and retry (token refresh
                    // may fix it on the next dial).
                    onSocketError(gen: gen)
                }
            }

        case .docUpdate(let crdt, _, let updates, _):
            await applyRemote(crdt: crdt, updates: updates)

        case .docUpdateFragmentHeader(let crdt, _, let batchId, let count, let total):
            guard count > 0, count <= RoomClient.maxFragmentCount,
                  total <= UInt64(RoomClient.maxReassembledBytes) else { return }
            fragments[batchId] = FragmentBuffer(crdt: crdt, parts: Array(repeating: nil, count: Int(count)),
                                                received: 0, totalSize: Int(total))

        case .docUpdateFragment(_, _, let batchId, let index, let fragment):
            await onFragment(batchId: batchId, index: Int(index), fragment: fragment)

        case .ack(let crdt, _, let refId, let status):
            await onAck(crdt: crdt, refId: refId, status: status)

        case .roomError(_, _, let code, _):
            if code == .evicted {
                onSocketError(gen: gen)
            } else {
                await sendJoinLoro(version: localVersionBytes())
            }

        case .joinRequest, .leave:
            return
        }
    }

    private func crdtOf(_ frame: ProtocolMessage) -> CrdtType {
        switch frame {
        case .joinRequest(let crdt, _, _, _),
             .joinResponseOk(let crdt, _, _, _, _),
             .joinError(let crdt, _, _, _),
             .docUpdate(let crdt, _, _, _),
             .docUpdateFragmentHeader(let crdt, _, _, _, _),
             .docUpdateFragment(let crdt, _, _, _, _),
             .roomError(let crdt, _, _, _),
             .ack(let crdt, _, _, _),
             .leave(let crdt, _):
            return crdt
        }
    }

    private func onJoinOk(crdt: CrdtType, version: [UInt8]) async {
        switch crdt {
        case .loro:
            joinSentAt = nil  // join answered — disarm the deadline
            let wasProbe = joinIsProbe
            joinIsProbe = false
            let advertisedVv: VersionVector
            if version.isEmpty {
                advertisedVv = VersionVector()
            } else if let decoded = try? VersionVector.decode(bytes: Data(version)) {
                advertisedVv = decoded
            } else {
                parkProtocol("invalid server version vector; parking room")
                return
            }
            serverVv = advertisedVv
            joinedLor = true
            // The version the room advertises IS proof of what it holds — the
            // strongest acknowledgement available, and the one a fresh client
            // would backfill from. Probe answers count too: that is how a room
            // converged by another device retires our outbox rows without
            // anything being sent.
            noteAcknowledged(advertisedVv)
            // Note what the join answer is worth and NOT more: it starts the
            // clock the reset is judged on, it is not the reset. This line used
            // to be `backoffMs = backoffBaseMs` (gh#405) — a DO that answers
            // the join and then dies answers it every dial, so the ladder was
            // pinned at 250ms for as long as the room stayed sick. First join
            // only: a probe answer or a rejoin mid-session must not restart the
            // session's lifetime.
            if joinedAt == nil { joinedAt = .now() }
            if wasProbe {
                // A probe or recovery answer on an established session proves
                // only that the room is alive and advertises the snapshot VV.
                // Do not re-upload the stale graph, re-join presence, or emit
                // another connected event; recovery replays only commands
                // after the replacement validates.
                return
            }
            // Resubmit-from-VV: push everything the server lacks. Gated on
            // the VERSION VECTORS, not the export bytes: the export returns a
            // non-empty envelope even when there is nothing to say, so a
            // byte-length gate made every liveness probe upload a no-op
            // DocUpdate that dirtied the room's caches (room.rs finding).
            if !doc.oplogVv().isEmpty(), invalidRejoins < RoomClient.maxInvalidRejoins {
                if !advertisedVv.includesVv(other: doc.oplogVv()),
                   let missing = try? doc.export(mode: .updates(from: advertisedVv)), !missing.isEmpty {
                    await sendLoroUpdates([[UInt8](missing)])
                }
            }
            roomLog.info("room \(self.roomId, privacy: .public): joined")
            // Join presence once the doc room is up.
            await send(.joinRequest(crdt: .loroEphemeral, roomId: roomId, auth: [], version: []))
            events(.connected)
        case .loroEphemeral:
            let all = eph.encodeAll()
            if !all.isEmpty {
                await send(.docUpdate(crdt: .loroEphemeral, roomId: roomId,
                                      updates: [[UInt8](all)], batchId: .random()))
            }
        }
    }

    private func applyRemote(crdt: CrdtType, updates: [[UInt8]]) async {
        switch crdt {
        case .loro:
            var imported = false
            var incomplete = false
            for update in updates where !update.isEmpty {
                do {
                    let status = try doc.importWith(bytes: Data(update), origin: "remote")
                    if status.pending == nil {
                        imported = true
                    } else {
                        incomplete = true
                        roomLog.error("room \(self.roomId, privacy: .public): remote import incomplete (\(status.pending?.count ?? 0) pending peer ranges); attempting snapshot reseed")
                        if await tryReseed(from: update) {
                            imported = true
                            incomplete = false
                        } else {
                            await requestFullResync()
                        }
                    }
                } catch {
                    incomplete = true
                    roomLog.error("room \(self.roomId, privacy: .public): remote update failed to import; requesting full snapshot resync")
                    await requestFullResync()
                }
            }
            if imported && !incomplete { events(.remoteUpdate) }
        case .loroEphemeral:
            var applied = false
            for update in updates where !update.isEmpty {
                if (try? eph.apply(data: Data(update))) != nil { applied = true }
            }
            if applied { events(.ephemeralUpdate) }
        }
    }

    private func requestFullResync() async {
        guard !fullResyncRequested else { return }
        fullResyncRequested = true
        await sendJoinLoro(version: [], suppressJoinEffects: joinedLor)
    }

    private func parkProtocol(_ reason: String) {
        guard !protocolParked else { return }
        protocolParked = true
        generation += 1
        receiveTask?.cancel()
        pingTask?.cancel()
        livenessTask?.cancel()
        socket?.cancel(with: .policyViolation, reason: Data(reason.utf8))
        socket = nil
        joinedLor = false
        joinedAt = nil
        roomLog.fault("room \(self.roomId, privacy: .public): \(reason, privacy: .public); explicit restart required")
        events(.disconnected)
    }

    /// Validate the server's snapshot, hand it to `ConvergenceRecovery` — which
    /// quarantines the stale document and replays every locally committed
    /// semantic entry onto the replacement — and swap the shared owner across.
    ///
    /// The ORDER is the safety property (gh#483): the quarantine is durable
    /// before anything is replaced, the replay and its verification happen
    /// before the swap is visible, and outbox rows survive until the edge's
    /// version proves it holds them. A failure anywhere before the swap leaves
    /// the stale document — unmergeable, but complete — in place.
    private func tryReseed(from snapshot: [UInt8]) async -> Bool {
        guard let serverVv else { return false }
        let replacement = LoroDoc()
        guard let status = try? replacement.importWith(bytes: Data(snapshot), origin: "remote-reseed"),
              status.pending == nil,
              replacement.oplogVv() == serverVv else { return false }
        guard let convergence else {
            // No journal, no quarantine, no way to put local content back.
            // Refusing is the only safe answer.
            roomLog.fault("room \(self.roomId, privacy: .public): no convergence journal; refusing to replace the local document")
            return false
        }

        // Anything derived from the stale graph is invalid. Clear it before
        // taking the owner gate; the recovery and the pointer swap are one
        // atomic operation with respect to SessionStore.queueCommand.
        pending.removeAll()
        invalidRejoins = 0
        publishConvergence()
        guard let report = document.replace(with: replacement, recover: { stale, candidate in
            convergence.recover(stale: stale, candidate: candidate, serverVersion: serverVv)
        }) else {
            roomLog.fault("room \(self.roomId, privacy: .public): convergence recovery refused the server snapshot; keeping the local document")
            blockedReason = "convergence recovery refused the server snapshot; local content is intact but unmerged"
            publishConvergence()
            return false
        }
        blockedReason = nil
        let replayUpdate: [UInt8]?
        if report.total > 0,
           let update = try? replacement.export(mode: .updates(from: serverVv)),
           !update.isEmpty {
            replayUpdate = [UInt8](update)
        } else {
            replayUpdate = nil
        }

        if let replayUpdate {
            await sendLoroUpdates([replayUpdate])
        }
        roomLog.error("room \(self.roomId, privacy: .public): reseeded stale local document from validated server snapshot; restored \(report.restoredMessages.count) message(s), extended \(report.extendedMessages.count), restored \(report.restoredCommands.count) command(s), resolved \(report.resolvedCommands.count), diverged \(report.diverged.count)")
        publishConvergence()
        return true
    }

    private func onFragment(batchId: BatchId, index: Int, fragment: [UInt8]) async {
        guard var buffer = fragments[batchId] else { return }
        guard index < buffer.parts.count else {
            fragments.removeValue(forKey: batchId)
            return
        }
        if buffer.parts[index] == nil { buffer.received += 1 }
        buffer.parts[index] = fragment
        if buffer.received < buffer.parts.count {
            fragments[batchId] = buffer
            return
        }
        fragments.removeValue(forKey: batchId)
        var total: [UInt8] = []
        total.reserveCapacity(buffer.totalSize)
        for part in buffer.parts { total.append(contentsOf: part ?? []) }
        await applyRemote(crdt: buffer.crdt, updates: [total])
    }

    private func onAck(crdt: CrdtType, refId: BatchId, status: UpdateStatusCode) async {
        switch status {
        case .ok:
            if let batch = pending.removeValue(forKey: refId), crdt == .loro,
               let version = batchVersion(batch) {
                noteAcknowledged(version)
            }
        case .fragmentTimeout:
            // DO hibernated mid-batch — resend the whole batch.
            if let batch = pending.removeValue(forKey: refId) {
                await sendLoroUpdates(batch)
            }
        case .invalidUpdate, .permissionDenied:
            guard pending.removeValue(forKey: refId) != nil else {
                // A delayed rejection for a pre-reseed batch is historical.
                return
            }
            roomLog.error("room \(self.roomId, privacy: .public): update rejected (\(String(describing: status), privacy: .public)); rejoining (\(self.invalidRejoins)/\(RoomClient.maxInvalidRejoins))")
            if crdt == .loro, invalidRejoins < RoomClient.maxInvalidRejoins {
                invalidRejoins += 1
                await sendJoinLoro(version: localVersionBytes())
            } else if crdt == .loro {
                // The room is live and will not take this device's history.
                // Before gh#483 that state was invisible — every surface said
                // "connected" while the document stopped converging for good.
                blockedReason = "the edge refuses this device's history (shallow start is newer) and no server snapshot has repaired it"
                publishConvergence()
            }
        default:
            roomLog.warning("room \(self.roomId, privacy: .public): ack status \(String(describing: status), privacy: .public)")
            pending.removeValue(forKey: refId)
        }
    }

    // MARK: Outbound

    /// Called by the doc store on local commit (subscribeLocalUpdate bytes).
    func sendLocalUpdate(_ update: [UInt8], generation: UInt64) async {
        guard generation == document.currentGeneration() else {
            // The shared owner was reseeded after this callback queued its
            // task. Replaying the old graph would immediately strand it again.
            return
        }
        guard joinedLor else {
            // Not lost — the commit is durable in the doc and the next
            // successful join resubmits from VV. But it IS invisible to the
            // user, so say so loudly (2026-07-31: sends queued behind a
            // failing join looked exactly like a working app).
            roomLog.warning("room \(self.roomId, privacy: .public): local update (\(update.count)B) deferred — not joined; will resubmit on join")
            return
        }
        await sendLoroUpdates([update])
    }

    /// Broadcast the presence store's local delta.
    func sendEphemeralUpdate(_ update: [UInt8]) async {
        guard joinedLor, !update.isEmpty else { return }
        await send(.docUpdate(crdt: .loroEphemeral, roomId: roomId, updates: [update], batchId: .random()))
    }

    private func sendJoinLoro(version: [UInt8], suppressJoinEffects: Bool = false) async {
        // Arm the answer deadline BEFORE the frame leaves: an unanswered join
        // used to hang the session forever (room.rs, 2026-07-30). Joins
        // default to non-probe; the probe branch in livenessTick flags itself
        // after this returns.
        joinSentAt = .now()
        joinIsProbe = suppressJoinEffects
        await send(.joinRequest(crdt: .loro, roomId: roomId, auth: [], version: version))
    }

    /// Batch small updates, fragment any single update above the payload budget.
    private func sendLoroUpdates(_ updates: [[UInt8]]) async {
        var small: [[UInt8]] = []
        var smallBytes = 0
        for update in updates where !update.isEmpty {
            if update.count > RoomClient.fragmentBytes {
                await sendFragmented(update)
                continue
            }
            if smallBytes + update.count > RoomClient.fragmentBytes {
                await sendBatch(small)
                small = []
                smallBytes = 0
            }
            small.append(update)
            smallBytes += update.count
        }
        if !small.isEmpty { await sendBatch(small) }
    }

    private func sendBatch(_ updates: [[UInt8]]) async {
        let batchId = BatchId.random()
        pending[batchId] = updates
        await send(.docUpdate(crdt: .loro, roomId: roomId, updates: updates, batchId: batchId))
    }

    private func sendFragmented(_ update: [UInt8]) async {
        let batchId = BatchId.random()
        pending[batchId] = [update]
        let chunks = stride(from: 0, to: update.count, by: RoomClient.fragmentBytes).map {
            Array(update[$0..<min($0 + RoomClient.fragmentBytes, update.count)])
        }
        await send(.docUpdateFragmentHeader(crdt: .loro, roomId: roomId, batchId: batchId,
                                            fragmentCount: UInt64(chunks.count),
                                            totalSizeBytes: UInt64(update.count)))
        for (ix, chunk) in chunks.enumerated() {
            await send(.docUpdateFragment(crdt: .loro, roomId: roomId, batchId: batchId,
                                          index: UInt64(ix), fragment: chunk))
        }
    }

    private func send(_ message: ProtocolMessage) async {
        guard let socket, let data = LoroWire.encode(message) else { return }
        try? await socket.send(.data(data))
    }

    private func localVersionBytes() -> [UInt8] {
        let vv = doc.oplogVv()
        return vv.isEmpty() ? [] : [UInt8](vv.encode())
    }
}

private extension VersionVector {
    func isEmpty() -> Bool {
        // An empty VV encodes to a fixed small header with no entries; the
        // cheapest reliable emptiness probe the FFI exposes is comparing
        // against a fresh VV.
        self == VersionVector()
    }
}
