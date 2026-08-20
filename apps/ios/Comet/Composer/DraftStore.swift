// Per-chat composer drafts, kept on the phone (gh#536).
//
// The desktop half of this is `crates/ui/src/drafts.rs`, and the rule is the
// same on both: what you typed belongs to the CHAT, not to the view that
// happened to be on screen when you typed it. A SwiftUI `@State` string dies
// with its view — and on a phone the view dies constantly: navigating to
// another chat, the question panel taking the composer's place, the system
// reclaiming a backgrounded app. Every one of those was a way to lose a
// half-written prompt, so the text lives here instead and the views only
// borrow it.
//
// **Local only.** Drafts are written to Application Support beside the doc
// snapshots and never enter a Loro doc: they are not transcript content, they
// do not sync, and they must not surface on another device.
//
// Cleared by exactly two things: sending, or the person emptying the box.

import Foundation
import SwiftUI

/// One chat's unsent composer contents. The `@` references are part of the
/// draft — a prompt that came back without them would come back changed.
struct Draft: Codable, Equatable {
    var text: String = ""
    var context: [ContextRef] = []
    /// Last write, epoch seconds — eviction order only, never displayed.
    var updatedAt: Double = 0

    var isEmpty: Bool { text.isEmpty && context.isEmpty }
}

@MainActor
@Observable
final class DraftStore {
    /// The app's store. Views reach it directly rather than through
    /// `AppModel`: a draft outlives sign-in state, org switches and every
    /// store the model rebuilds.
    static let shared = DraftStore()

    /// How many chats keep a draft. Far above any plausible number of
    /// half-written prompts; the cap only stops a long-lived install from
    /// growing the file forever. Oldest-first.
    static let maxDrafts = 200

    /// Debounce before writing after a keystroke.
    static let saveDelay = Duration.milliseconds(250)

    /// The draft key for a live chat, and for a space's new-session canvas —
    /// which has no chat yet, and whose text belongs to that space and no
    /// other. (Same scheme as the desktop's `draft_key`.)
    static func key(chat id: String) -> String { id }
    static func key(newSessionIn spaceId: String) -> String { "new:\(spaceId)" }

    private var drafts: [String: Draft] = [:]
    private let fileURL: URL
    @ObservationIgnored private var saveTask: Task<Void, Never>?

    nonisolated static var defaultURL: URL {
        let base = FileManager.default.urls(for: .applicationSupportDirectory,
                                            in: .userDomainMask)[0]
            .appendingPathComponent("CometDrafts", isDirectory: true)
        try? FileManager.default.createDirectory(at: base, withIntermediateDirectories: true)
        return base.appendingPathComponent("drafts.json")
    }

    init(fileURL: URL = DraftStore.defaultURL) {
        self.fileURL = fileURL
        // A corrupt or half-written file starts empty rather than throwing: a
        // draft file is a convenience, and refusing to open the app over one
        // would be worse than the loss it exists to prevent.
        if let data = try? Data(contentsOf: fileURL),
           let loaded = try? JSONDecoder().decode([String: Draft].self, from: data) {
            drafts = loaded.filter { !$0.value.isEmpty }
            evict()
        }
    }

    // MARK: Reading

    func draft(for key: String) -> Draft { drafts[key] ?? Draft() }
    func text(for key: String) -> String { drafts[key]?.text ?? "" }
    func context(for key: String) -> [ContextRef] { drafts[key]?.context ?? [] }
    var count: Int { drafts.count }

    /// The composer's binding. Reading it inside a view body registers the
    /// observation, so a draft restored from disk paints without a nudge.
    func textBinding(for key: String) -> Binding<String> {
        Binding(get: { self.text(for: key) }, set: { self.setText($0, for: key) })
    }

    // MARK: Writing

    func setText(_ text: String, for key: String) {
        update(key) { $0.text = text }
    }

    func setContext(_ context: [ContextRef], for key: String) {
        update(key) { $0.context = context }
    }

    /// Sending it, or the person emptying it — the only two ways a draft goes.
    func clear(_ key: String) {
        guard drafts.removeValue(forKey: key) != nil else { return }
        scheduleSave()
    }

    private func update(_ key: String, _ change: (inout Draft) -> Void) {
        var draft = drafts[key] ?? Draft()
        let before = draft
        change(&draft)
        guard draft != before else { return }
        if draft.isEmpty {
            drafts.removeValue(forKey: key)
        } else {
            draft.updatedAt = Date().timeIntervalSince1970
            drafts[key] = draft
            evict()
        }
        scheduleSave()
    }

    private func evict() {
        guard drafts.count > Self.maxDrafts else { return }
        let doomed = drafts
            .sorted { ($0.value.updatedAt, $0.key) < ($1.value.updatedAt, $1.key) }
            .prefix(drafts.count - Self.maxDrafts)
        for (key, _) in doomed { drafts.removeValue(forKey: key) }
    }

    // MARK: Persistence

    private func scheduleSave() {
        saveTask?.cancel()
        saveTask = Task { [weak self] in
            try? await Task.sleep(for: DraftStore.saveDelay)
            guard !Task.isCancelled else { return }
            self?.saveNow()
        }
    }

    /// Write now, on the way out: the debounce is a task, and a task does not
    /// survive an app the system decided to reclaim. Called on the way to the
    /// background, where a phone's "quit" actually happens.
    func flush() {
        saveTask?.cancel()
        saveTask = nil
        saveNow()
    }

    private func saveNow() {
        guard let data = try? JSONEncoder().encode(drafts) else { return }
        // Atomic: a crash mid-write must not eat what was already saved.
        try? data.write(to: fileURL, options: .atomic)
    }
}
