// Convergence recovery — the phone's copy of `crates/sync/src/convergence.rs`.
//
// It has to be a copy: no Rust runs on this device, and the room loop it hangs
// off (`RoomClient.swift`) is already a hand-mirrored port of `room.rs`. Read
// the Rust module first — it carries the full argument. The short version:
//
//   No locally committed semantic entry may be discarded until the edge has
//   acknowledged it and a fresh independent client can retrieve its stable id.
//
// gh#450 taught both clients to reseed from a server snapshot their local
// history could no longer merge with. What crossed that cut was the local
// device's unresolved COMMAND intent and nothing else, so a replica holding
// transcript entries the room had lost could only choose between staying
// permanently unmergeable and dropping them (gh#483). Three things fix that,
// here exactly as in Rust:
//
//   1. a durable QUARANTINE of the whole pre-replacement document, retained
//      until a fresh client proves convergence;
//   2. a durable semantic OUTBOX — entries and commands as content, with their
//      parts, timestamps, statuses, continuations, attachment refs and
//      provenance — which survives losing the CRDT graph that carried them;
//   3. REPLAY that is idempotent by stable id, so a crash mid-recovery is
//      finished by the next launch instead of duplicating or losing anything.
//
// Behaviour is held to the Rust file by `crates/sync/tests/ios_room.rs`, which
// reads both as text on every push. The phone deliberately differs in ONE
// place: its journal is files under Application Support rather than SQLite,
// because that is what the rest of this app's persistence is.

import Foundation
import Loro
import os

let convergenceLog = Logger(subsystem: "dev.cometnative.Comet", category: "convergence")

/// How long a room may stay transport-live with unacknowledged local content
/// before that is a fault rather than lag. Mirrors `UNACKNOWLEDGED_ALERT`.
let convergenceUnacknowledgedAlertNs: UInt64 = 120_000_000_000

// MARK: - Semantic content

enum SemanticKind: String, Codable {
    case message
    case command

    var containerName: String {
        switch self {
        case .message: return "messages"
        case .command: return "commands"
        }
    }
}

/// One locally committed semantic mutation, in the form that survives losing
/// the history that carried it. `payload` is the entry itself as JSON;
/// `version` is the document's oplog version vector when it was recorded — the
/// edge holds this content once its advertised version includes that vector.
struct SemanticMutation: Codable, Equatable {
    var kind: SemanticKind
    var stableId: String
    var payload: Data
    var version: Data
    var seq: Int

    var json: [String: Any]? {
        (try? JSONSerialization.jsonObject(with: payload)) as? [String: Any]
    }

    /// Conservative on purpose: undecodable version bytes are never read as
    /// "the edge has it", because that is the one mistake that discards
    /// content.
    func acknowledged(by acked: VersionVector) -> Bool {
        guard let recorded = try? VersionVector.decode(bytes: version) else { return false }
        return acked.includesVv(other: recorded)
    }
}

/// What one replay pass did — ids, so a log line and a test name the same
/// things (mirrors `ReplayReport`).
struct ReplayReport: Equatable {
    var restoredMessages: [String] = []
    var extendedMessages: [String] = []
    var restoredCommands: [String] = []
    var resolvedCommands: [String] = []
    /// Ids where the authoritative copy carries MORE than the local one. It
    /// wins; the quarantine keeps the local bytes.
    var diverged: [String] = []

    var total: Int {
        restoredMessages.count + extendedMessages.count
            + restoredCommands.count + resolvedCommands.count
    }

    var isEmpty: Bool { total == 0 && diverged.isEmpty }
}

// MARK: - Truthful per-room state

/// What a room can honestly say about its CONTENT, as opposed to its socket.
/// The gh#483 incident is exactly the gap between the two.
enum ConvergenceState: Equatable {
    case converged
    case pending(unacked: Int, stalled: Bool)
    case recovering(phase: RecoveryPhase, unacked: Int)
    case blockedLocalOnly(unacked: Int, reason: String)

    var isConverged: Bool { self == .converged }

    var unacked: Int {
        switch self {
        case .converged: return 0
        case .pending(let n, _): return n
        case .recovering(_, let n): return n
        case .blockedLocalOnly(let n, _): return n
        }
    }

    var needsAttention: Bool {
        switch self {
        case .blockedLocalOnly: return true
        case .pending(_, let stalled): return stalled
        default: return false
        }
    }

    /// One phrase for a status line. Never says "live".
    var label: String {
        switch self {
        case .converged: return "converged"
        case .pending(let n, false): return "pending \(n)"
        case .pending(let n, true):
            return "pending \(n) — unacknowledged past the alert threshold"
        case .recovering(let phase, let n):
            return "recovering (\(phase.rawValue), \(n) local-only)"
        case .blockedLocalOnly(let n, let reason):
            return "blocked/local-only (\(n) local-only: \(reason))"
        }
    }
}

/// Durable phase of an in-flight recovery. Ordered: anything below
/// `.acknowledged` replays from the quarantine on the next launch.
enum RecoveryPhase: String, Codable, Comparable {
    case quarantined
    case reseeded
    case replayed
    case acknowledged

    var rank: Int {
        switch self {
        case .quarantined: return 0
        case .reseeded: return 1
        case .replayed: return 2
        case .acknowledged: return 3
        }
    }

    static func < (lhs: RecoveryPhase, rhs: RecoveryPhase) -> Bool { lhs.rank < rhs.rank }
}

/// One open recovery, as it survives a relaunch.
struct RecoveryRecord: Codable, Equatable {
    var docId: String
    var phase: RecoveryPhase
    /// Snapshot of the document as it stood before replacement, on disk.
    var quarantine: Data
    var serverVersion: Data
    /// Every stable id that must still be retrievable when this is over.
    var expectedIds: [String]
    var startedAt: Int64
}

// MARK: - The journal

protocol ConvergenceJournal: AnyObject, Sendable {
    func record(docId: String, mutations: [SemanticMutation]) -> Int
    /// Re-key rows onto a new document's version whether or not their content
    /// changed — what a completed replay needs and `record` must not do: the
    /// abandoned graph's version can never be acknowledged, because the edge
    /// refused precisely those operations.
    func rebase(docId: String, mutations: [SemanticMutation]) -> Int
    func unacknowledged(docId: String) -> [SemanticMutation]
    func acknowledge(docId: String, acked: VersionVector) -> Int
    func beginRecovery(_ record: RecoveryRecord)
    func advanceRecovery(docId: String, phase: RecoveryPhase)
    func openRecovery(docId: String) -> RecoveryRecord?
    func releaseRecovery(docId: String)
}

/// File-backed journal under Application Support, beside the doc snapshots it
/// protects (`DocDisk`). Atomic writes: a half-written outbox is the one thing
/// that could turn this from a safety net into a lie.
final class FileConvergenceJournal: ConvergenceJournal, @unchecked Sendable {
    static let shared = FileConvergenceJournal()

    private let lock = NSLock()
    /// Spec-only: a directory of its own, so `SyncSpecRunner` can simulate a
    /// relaunch (a second instance over the same directory) without touching
    /// the real device's outbox.
    private let root: URL?

    init(root: URL? = nil) {
        self.root = root
    }

    private var directory: URL {
        let base = (root ?? DocDisk.directory)
            .appendingPathComponent("convergence", isDirectory: true)
        try? FileManager.default.createDirectory(at: base, withIntermediateDirectories: true)
        return base
    }

    private func safe(_ docId: String) -> String {
        docId.replacingOccurrences(of: "/", with: "_")
    }

    private func outboxURL(_ docId: String) -> URL {
        directory.appendingPathComponent("outbox-\(safe(docId)).json")
    }

    private func recoveryURL(_ docId: String) -> URL {
        directory.appendingPathComponent("recovery-\(safe(docId)).json")
    }

    private func loadOutbox(_ docId: String) -> [SemanticMutation] {
        guard let data = try? Data(contentsOf: outboxURL(docId)),
              let rows = try? JSONDecoder().decode([SemanticMutation].self, from: data)
        else { return [] }
        return rows
    }

    private func writeOutbox(_ docId: String, _ rows: [SemanticMutation]) {
        guard let data = try? JSONEncoder().encode(rows) else { return }
        try? data.write(to: outboxURL(docId), options: .atomic)
    }

    func record(docId: String, mutations: [SemanticMutation]) -> Int {
        lock.lock()
        defer { lock.unlock() }
        var rows = loadOutbox(docId)
        var index: [String: Int] = [:]
        for (position, row) in rows.enumerated() {
            index["\(row.kind.rawValue)/\(row.stableId)"] = position
        }
        var written = 0
        for mutation in mutations {
            let key = "\(mutation.kind.rawValue)/\(mutation.stableId)"
            if let position = index[key] {
                // Unchanged content keeps its ORIGINAL version: re-stamping it
                // on every save would make every entry wait on the newest local
                // operation, and the pending count would be true but useless.
                if rows[position].payload == mutation.payload { continue }
                rows[position] = mutation
            } else {
                index[key] = rows.count
                rows.append(mutation)
            }
            written += 1
        }
        if written > 0 { writeOutbox(docId, rows) }
        return written
    }

    func rebase(docId: String, mutations: [SemanticMutation]) -> Int {
        lock.lock()
        defer { lock.unlock() }
        var rows = loadOutbox(docId)
        var index: [String: Int] = [:]
        for (position, row) in rows.enumerated() {
            index["\(row.kind.rawValue)/\(row.stableId)"] = position
        }
        for mutation in mutations {
            let key = "\(mutation.kind.rawValue)/\(mutation.stableId)"
            if let position = index[key] {
                rows[position] = mutation
            } else {
                index[key] = rows.count
                rows.append(mutation)
            }
        }
        writeOutbox(docId, rows)
        return mutations.count
    }

    func unacknowledged(docId: String) -> [SemanticMutation] {
        lock.lock()
        defer { lock.unlock() }
        return loadOutbox(docId).sorted {
            ($0.kind.rawValue, $0.seq) < ($1.kind.rawValue, $1.seq)
        }
    }

    func acknowledge(docId: String, acked: VersionVector) -> Int {
        lock.lock()
        defer { lock.unlock() }
        let rows = loadOutbox(docId)
        let keep = rows.filter { !$0.acknowledged(by: acked) }
        if keep.count == rows.count { return 0 }
        writeOutbox(docId, keep)
        return rows.count - keep.count
    }

    func beginRecovery(_ record: RecoveryRecord) {
        lock.lock()
        defer { lock.unlock() }
        guard let data = try? JSONEncoder().encode(record) else { return }
        try? data.write(to: recoveryURL(record.docId), options: .atomic)
    }

    func advanceRecovery(docId: String, phase: RecoveryPhase) {
        lock.lock()
        defer { lock.unlock() }
        guard var record = loadRecovery(docId) else { return }
        record.phase = phase
        guard let data = try? JSONEncoder().encode(record) else { return }
        try? data.write(to: recoveryURL(docId), options: .atomic)
    }

    func openRecovery(docId: String) -> RecoveryRecord? {
        lock.lock()
        defer { lock.unlock() }
        return loadRecovery(docId)
    }

    func releaseRecovery(docId: String) {
        lock.lock()
        defer { lock.unlock() }
        try? FileManager.default.removeItem(at: recoveryURL(docId))
    }

    private func loadRecovery(_ docId: String) -> RecoveryRecord? {
        guard let data = try? Data(contentsOf: recoveryURL(docId)) else { return nil }
        return try? JSONDecoder().decode(RecoveryRecord.self, from: data)
    }
}

// MARK: - Reading and writing semantic content

enum Convergence {
    /// Every semantic mutation the document currently carries, in list order —
    /// how the quarantine becomes replayable content, and how a device that has
    /// never journaled anything still recovers everything it holds.
    static func semanticMutations(_ doc: LoroDoc) -> [SemanticMutation] {
        let version = doc.oplogVv().encode()
        var out: [SemanticMutation] = []
        for kind in [SemanticKind.message, SemanticKind.command] {
            let list = doc.getList(id: kind.containerName).getDeepValue().listValue ?? []
            for (index, value) in list.enumerated() {
                guard let map = value.mapValue,
                      let id = map["id"]?.stringValue,
                      let payload = try? JSONSerialization.data(
                          withJSONObject: map.mapValues(\.jsonObject))
                else { continue }
                out.append(SemanticMutation(kind: kind, stableId: id, payload: payload,
                                            version: version, seq: index))
            }
        }
        return out
    }

    /// Stable ids a recovery must still be able to retrieve afterwards.
    static func stableIds(_ doc: LoroDoc) -> [String] {
        var ids: [String] = []
        for kind in [SemanticKind.message, SemanticKind.command] {
            let list = doc.getList(id: kind.containerName).getDeepValue().listValue ?? []
            ids.append(contentsOf: list.compactMap { $0.mapValue?["id"]?.stringValue })
        }
        return ids
    }

    /// Replay `mutations` onto `target`, idempotently, keyed by stable id.
    ///
    /// Same four rules as `replay_into` in Rust, in the same order: absent →
    /// commit verbatim; equal → nothing; local carries at least as much → the
    /// authoritative copy is a truncated prefix, rewrite it; authoritative
    /// carries more → leave it and report divergence, because rolling a peer's
    /// later content back is the one way replay could destroy data.
    static func replayInto(_ target: LoroDoc, mutations: [SemanticMutation]) -> ReplayReport? {
        var report = ReplayReport()
        var existing: [String: [String: Any]] = [:]
        for kind in [SemanticKind.message, SemanticKind.command] {
            let list = target.getList(id: kind.containerName).getDeepValue().listValue ?? []
            for value in list {
                guard let map = value.mapValue, let id = map["id"]?.stringValue else { continue }
                existing["\(kind.rawValue)/\(id)"] = map.mapValues(\.jsonObject)
            }
        }
        let ordered = mutations.sorted { ($0.kind.rawValue, $0.seq) < ($1.kind.rawValue, $1.seq) }
        do {
            for mutation in ordered {
                guard let local = mutation.json else { continue }
                let key = "\(mutation.kind.rawValue)/\(mutation.stableId)"
                guard let remote = existing[key] else {
                    try append(mutation.kind, local, into: target)
                    existing[key] = local
                    switch mutation.kind {
                    case .message: report.restoredMessages.append(mutation.stableId)
                    case .command: report.restoredCommands.append(mutation.stableId)
                    }
                    continue
                }
                if sameContent(remote, local) { continue }
                switch mutation.kind {
                case .message:
                    let remoteParts = (remote["parts"] as? [Any])?.count ?? 0
                    let localParts = (local["parts"] as? [Any])?.count ?? 0
                    if remoteParts <= localParts {
                        if try overwriteMessage(mutation.stableId, local, in: target) {
                            existing[key] = local
                            report.extendedMessages.append(mutation.stableId)
                        }
                    } else {
                        report.diverged.append(mutation.stableId)
                    }
                case .command:
                    // Ledger rule 2: an outcome may be written onto a Pending
                    // row; a settled outcome is never overwritten by another.
                    let remoteStatus = remote["status"] as? String
                    let localStatus = local["status"] as? String
                    if remoteStatus == "pending", let localStatus, localStatus != "pending" {
                        if try setCommandStatus(mutation.stableId, local, in: target) {
                            existing[key] = local
                            report.resolvedCommands.append(mutation.stableId)
                        }
                    } else {
                        report.diverged.append(mutation.stableId)
                    }
                }
            }
        } catch {
            convergenceLog.fault("convergence replay failed: \(String(describing: error), privacy: .public)")
            return nil
        }
        target.commit()
        return report
    }

    private static func sameContent(_ lhs: [String: Any], _ rhs: [String: Any]) -> Bool {
        guard let a = try? JSONSerialization.data(withJSONObject: lhs, options: [.sortedKeys]),
              let b = try? JSONSerialization.data(withJSONObject: rhs, options: [.sortedKeys])
        else { return false }
        return a == b
    }

    /// Append one entry to its list, in the doc schema `schema.rs` writes:
    /// scalar fields as map values, `parts` as a list container, and every
    /// text body as a LoroText container (never an LWW value rewrite — that
    /// shape is what keeps the oplog at 1.03× the transcript).
    private static func append(_ kind: SemanticKind, _ entry: [String: Any],
                               into target: LoroDoc) throws {
        let list = target.getList(id: kind.containerName)
        let map = try list.pushContainer(child: LoroMap())
        try write(entry: entry, into: map)
    }

    private static func write(entry: [String: Any], into map: LoroMap) throws {
        for (key, value) in entry where key != "parts" {
            try map.insert(key: key, v: LoroValue.fromJSON(value))
        }
        guard let parts = entry["parts"] as? [Any] else { return }
        let list = try map.insertContainer(key: "parts", child: LoroList())
        for part in parts {
            guard let part = part as? [String: Any] else { continue }
            let partMap = try list.pushContainer(child: LoroMap())
            for (key, value) in part where key != "text" {
                try partMap.insert(key: key, v: LoroValue.fromJSON(value))
            }
            if let text = part["text"] as? String {
                let body = try partMap.insertContainer(key: "text", child: LoroText())
                try body.insert(pos: 0, s: text)
            }
        }
    }

    /// Rewrite an entry the authoritative snapshot carries in truncated form.
    /// A fresh parts container rather than an in-place edit: the remote copy
    /// may hold parts this device never had, and a partial overwrite would
    /// interleave the two.
    private static func overwriteMessage(_ id: String, _ entry: [String: Any],
                                         in target: LoroDoc) throws -> Bool {
        let list = target.getList(id: SemanticKind.message.containerName)
        for index in 0..<list.len() {
            guard let map = list.get(index: index)?.asLoroMap(),
                  map.get(key: "id")?.asValue()?.stringValue == id else { continue }
            try write(entry: entry, into: map)
            return true
        }
        return false
    }

    private static func setCommandStatus(_ id: String, _ entry: [String: Any],
                                         in target: LoroDoc) throws -> Bool {
        let list = target.getList(id: SemanticKind.command.containerName)
        for index in 0..<list.len() {
            guard let map = list.get(index: index)?.asLoroMap(),
                  map.get(key: "id")?.asValue()?.stringValue == id else { continue }
            if let status = entry["status"] as? String {
                try map.insert(key: "status", v: LoroValue.string(value: status))
            }
            if let resolution = entry["resolution"] as? String {
                try map.insert(key: "resolution", v: LoroValue.string(value: resolution))
            }
            return true
        }
        return false
    }
}

// MARK: - The driver

/// The room-sync seam's recovery driver: the quarantine, the replay, the
/// acknowledgement accounting, and the truthful state — one object, mirroring
/// `ConvergenceRecovery` in Rust phase for phase.
final class ConvergenceRecovery: @unchecked Sendable {
    let docId: String
    let journal: ConvergenceJournal

    init(docId: String, journal: ConvergenceJournal = FileConvergenceJournal.shared) {
        self.docId = docId
        self.journal = journal
    }

    /// Record locally committed content into the semantic outbox — called on
    /// the same beat as the snapshot save, because that is what makes local
    /// content durable.
    @discardableResult
    func record(_ doc: LoroDoc) -> Int {
        journal.record(docId: docId, mutations: Convergence.semanticMutations(doc))
    }

    @discardableResult
    private func rebase(_ doc: LoroDoc) -> Int {
        journal.rebase(docId: docId, mutations: Convergence.semanticMutations(doc))
    }

    /// Phases 1–3: quarantine the stale document, then replay everything the
    /// candidate lacks onto the candidate.
    ///
    /// The caller has already validated that `candidate` is exactly the
    /// document the edge advertised. Nothing is swapped here; returning nil
    /// leaves the stale document in place, which is the half of the fork that
    /// loses nothing.
    func recover(stale: LoroDoc, candidate: LoroDoc, serverVersion: VersionVector) -> ReplayReport? {
        guard let quarantine = try? stale.export(mode: .snapshot) else {
            convergenceLog.fault("could not quarantine \(self.docId, privacy: .public); refusing to replace it")
            return nil
        }
        let outbox = journal.unacknowledged(docId: docId)
        var expected = Set(Convergence.stableIds(stale))
        expected.formUnion(Convergence.stableIds(candidate))
        expected.formUnion(outbox.map(\.stableId))
        let record = RecoveryRecord(
            docId: docId, phase: .quarantined, quarantine: quarantine,
            serverVersion: serverVersion.encode(), expectedIds: expected.sorted(),
            startedAt: Int64(Date().timeIntervalSince1970 * 1000))
        journal.beginRecovery(record)

        var mutations = Convergence.semanticMutations(stale)
        let known = Set(mutations.map { "\($0.kind.rawValue)/\($0.stableId)" })
        var nextSeq = (mutations.map(\.seq).max() ?? -1) + 1
        for row in outbox where !known.contains("\(row.kind.rawValue)/\(row.stableId)") {
            var rebased = row
            rebased.seq = nextSeq
            nextSeq += 1
            mutations.append(rebased)
        }

        journal.advanceRecovery(docId: docId, phase: .reseeded)
        guard let report = Convergence.replayInto(candidate, mutations: mutations),
              verify(candidate, expected: record.expectedIds)
        else { return nil }
        rebase(candidate)
        journal.advanceRecovery(docId: docId, phase: .replayed)
        return report
    }

    /// Launch path: finish whatever a crash interrupted. Cheap when there is
    /// nothing to do, and idempotent by stable id when there is.
    @discardableResult
    func resume(into current: LoroDoc) -> ReplayReport? {
        guard let record = journal.openRecovery(docId: docId) else { return nil }
        let quarantined = LoroDoc()
        guard let status = try? quarantined.importWith(bytes: record.quarantine,
                                                       origin: "quarantine"),
              status.pending == nil else {
            convergenceLog.fault("quarantined snapshot for \(self.docId, privacy: .public) is incomplete; refusing to resume from it")
            return nil
        }
        var mutations = Convergence.semanticMutations(quarantined)
        let known = Set(mutations.map { "\($0.kind.rawValue)/\($0.stableId)" })
        var nextSeq = (mutations.map(\.seq).max() ?? -1) + 1
        for row in journal.unacknowledged(docId: docId)
        where !known.contains("\(row.kind.rawValue)/\(row.stableId)") {
            var rebased = row
            rebased.seq = nextSeq
            nextSeq += 1
            mutations.append(rebased)
        }
        guard let report = Convergence.replayInto(current, mutations: mutations),
              verify(current, expected: record.expectedIds) else { return nil }
        rebase(current)
        if record.phase < .replayed {
            journal.advanceRecovery(docId: docId, phase: .replayed)
        }
        convergenceLog.error("resumed an interrupted convergence recovery for \(self.docId, privacy: .public) at phase \(record.phase.rawValue, privacy: .public); replayed \(report.total)")
        return report
    }

    /// The edge advertised `acked`: drop the outbox rows it proves are held,
    /// and promote a replayed recovery to `.acknowledged`.
    @discardableResult
    func acknowledge(_ acked: VersionVector) -> Int {
        let dropped = journal.acknowledge(docId: docId, acked: acked)
        if let record = journal.openRecovery(docId: docId), record.phase == .replayed,
           journal.unacknowledged(docId: docId).isEmpty {
            journal.advanceRecovery(docId: docId, phase: .acknowledged)
        }
        return dropped
    }

    /// A fresh independent client retrieved `observed`. Releases the quarantine
    /// only when every expected id is among them — acknowledgement is the
    /// edge's word for it, and the invariant asks for someone else's.
    /// Returns the ids still missing (empty = released), or nil when there was
    /// no open recovery.
    @discardableResult
    func confirmIndependent(_ observed: [String]) -> [String]? {
        guard let record = journal.openRecovery(docId: docId) else { return nil }
        let seen = Set(observed)
        let missing = record.expectedIds.filter { !seen.contains($0) }
        guard missing.isEmpty else {
            convergenceLog.fault("independent verification of \(self.docId, privacy: .public) is short \(missing.count) id(s); keeping the quarantine")
            return missing
        }
        journal.releaseRecovery(docId: docId)
        convergenceLog.info("convergence of \(self.docId, privacy: .public) verified by an independent client; quarantine released")
        return []
    }

    var pending: Int { journal.unacknowledged(docId: docId).count }

    var openRecovery: RecoveryRecord? { journal.openRecovery(docId: docId) }

    /// Local completeness proof: a document built from nothing but the
    /// candidate's exported bytes still answers for every expected id.
    private func verify(_ candidate: LoroDoc, expected: [String]) -> Bool {
        guard let bytes = try? candidate.export(mode: .snapshot) else { return false }
        let fresh = LoroDoc()
        guard let status = try? fresh.importWith(bytes: bytes, origin: "verify"),
              status.pending == nil else { return false }
        let present = Set(Convergence.stableIds(fresh))
        let missing = expected.filter { !present.contains($0) }
        guard missing.isEmpty else {
            convergenceLog.fault("recovered document for \(self.docId, privacy: .public) is missing \(missing.count) stable id(s); refusing the replacement")
            return false
        }
        return true
    }
}
