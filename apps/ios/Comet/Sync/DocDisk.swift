// On-device Loro doc persistence — the old mobile app's snapshot cache
// (kv.ts/loro-room.ts) and the engine's DocsStore, in file form: one snapshot
// per doc under Application Support. Docs load BEFORE the room join, so the
// UI renders instantly from local state (offline included) and the join's
// version vector turns the backfill incremental instead of a full snapshot.

import Foundation
import Loro

enum DocDisk {
    static var directory: URL {
        let base = FileManager.default.urls(for: .applicationSupportDirectory,
                                            in: .userDomainMask)[0]
            .appendingPathComponent("CometDocs", isDirectory: true)
        try? FileManager.default.createDirectory(at: base, withIntermediateDirectories: true)
        return base
    }

    static func url(for id: String) -> URL {
        let safe = id.replacingOccurrences(of: "/", with: "_")
        return directory.appendingPathComponent("\(safe).loro")
    }

    /// On-disk id for the per-user workspace doc — deliberately NOT the room
    /// name (gh#148).
    ///
    /// A room generation bump (`ws3/…` → `ws4/…`) abandons the edge's storage
    /// on purpose, and that is only safe because every device still holds the
    /// doc locally to re-seed the virgin room from. Keying this file by the
    /// room name made the local copy die WITH the room it was supposed to
    /// outlive: a bump would have orphaned the snapshot, cost the sidebar its
    /// instant local-first render, and lost any edit made while offline. The
    /// desktop engine has always kept these separate (`WORKSPACE_DOC_ID =
    /// "workspace2"` is stable across every room generation); this matches it,
    /// name and all.
    static func workspaceId(orgId: String, userId: String) -> String {
        "workspace2/\(orgId)/\(userId)"
    }

    /// Hydrate the workspace doc, adopting the pre-gh#148 file if that is all
    /// this device has — the one-time cost of having keyed it by room name.
    @discardableResult
    static func loadWorkspace(into doc: LoroDoc, orgId: String, userId: String) -> Bool {
        let id = workspaceId(orgId: orgId, userId: userId)
        if load(into: doc, id: id) { return true }
        let legacy = "ws3/\(orgId)/\(userId)"
        guard load(into: doc, id: legacy) else { return false }
        // Adopt it under the stable name, then drop the old file: left behind,
        // it no longer matches the keep-rule in `prune` and would be LRU-evicted
        // as if it were a session snapshot.
        save(doc: doc, id: id)
        try? FileManager.default.removeItem(at: url(for: legacy))
        return true
    }

    /// Import the saved snapshot, if any. Returns whether anything loaded.
    @discardableResult
    static func load(into doc: LoroDoc, id: String) -> Bool {
        guard let data = try? Data(contentsOf: url(for: id)), !data.isEmpty else { return false }
        // Validate away from the live owner. Loro can return a non-throwing
        // pending status after mutating its target, and carrying that partial
        // state into the room just recreates the merge trap recovery handles.
        let candidate = LoroDoc()
        guard let candidateStatus = try? candidate.importWith(bytes: data, origin: "disk-probe"),
              candidateStatus.pending == nil,
              let installedStatus = try? doc.importWith(bytes: data, origin: "disk"),
              installedStatus.pending == nil else { return false }
        return true
    }

    /// Atomically persist the doc's snapshot.
    static func save(doc: LoroDoc, id: String) {
        guard let data = try? doc.export(mode: .snapshot) else { return }
        try? data.write(to: url(for: id), options: .atomic)
    }

    /// LRU-prune session snapshots (the workspace doc is always kept).
    ///
    /// The keep-rule matches the STABLE workspace file name, not a room
    /// generation. It used to spell `ws3_`, which made it one more guard that a
    /// generation bump would have silently switched off (gh#148) — and this one
    /// fails destructively: an unprotected workspace snapshot is not merely
    /// force-trimmed, it is deleted as the 81st-oldest session.
    static func prune(keep: Int) {
        let fm = FileManager.default
        guard let files = try? fm.contentsOfDirectory(at: directory,
                                                      includingPropertiesForKeys: [.contentModificationDateKey])
        else { return }
        let sessions = files.filter { !$0.lastPathComponent.hasPrefix("workspace2_") }
        guard sessions.count > keep else { return }
        let sorted = sessions.sorted {
            let a = (try? $0.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate) ?? .distantPast
            let b = (try? $1.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate) ?? .distantPast
            return a > b
        }
        for stale in sorted.dropFirst(keep) {
            try? fm.removeItem(at: stale)
        }
    }

    /// Sign-out hygiene: local doc state belongs to the signed-in identity.
    static func wipeAll() {
        try? FileManager.default.removeItem(at: directory)
    }
}

/// Debounced snapshot persistence shared by the doc stores: poke on every
/// change; the snapshot writes ~1.5s after the last poke, and `flush` forces
/// it (backgrounding, store teardown).
@MainActor
final class DocSaver {
    private let docId: String
    private let document: RoomDocument
    /// The semantic outbox is written on the same beat as the snapshot: what
    /// makes local content durable is what has to account for it (gh#483 §2).
    private let convergence: ConvergenceRecovery?
    /// Document version at the last journal record. Semantic content cannot
    /// change without the version changing, so an unchanged version skips the
    /// scan — the common case for an open-but-idle chat.
    private var journaledVersion: Data?
    private var generation = 0
    private var dirty = false

    init(docId: String, document: RoomDocument, convergence: ConvergenceRecovery? = nil) {
        self.docId = docId
        self.document = document
        self.convergence = convergence
    }

    func poke() {
        dirty = true
        generation += 1
        let expected = generation
        Task { @MainActor [weak self] in
            try? await Task.sleep(nanoseconds: 1_500_000_000)
            guard let self, self.generation == expected else { return }
            self.flush()
        }
    }

    func flush() {
        guard dirty else { return }
        dirty = false
        let doc = document.current()
        DocDisk.save(doc: doc, id: docId)
        guard let convergence else { return }
        let version = doc.oplogVv().encode()
        guard version != journaledVersion else { return }
        convergence.record(doc)
        journaledVersion = version
    }
}
