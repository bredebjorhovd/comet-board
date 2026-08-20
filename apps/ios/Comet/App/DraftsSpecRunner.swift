// The phone's composer drafts, asserted — launch with `-drafts-spec` (or run
// `scripts/ios-drafts-spec.sh`, which does the whole loop).
//
// gh#536: a draft is a per-chat thing that survives navigation, backgrounding
// and a cold launch, and is cleared by exactly two things — sending it, or the
// person emptying the box. On the phone all four of those used to be a
// `@State` string that died with the view, so what this checks is the store
// that replaced it: keys, isolation between chats, the two clears, and the
// round trip through the file that makes a relaunch remember.
//
// A launch-arg runner rather than XCTest for the reason `SpecRunner` gives at
// length: one target, one shared scheme, and a test target means editing the
// two files the operator keeps local changes in. This half is NOT in CI (it
// needs a simulator); the desktop half of the same rules — `crates/ui/src/
// drafts.rs` and the composer's `#[gpui::test]`s — runs on every push.

import Foundation

@MainActor
enum DraftsSpecRunner {
    static var logURL: URL {
        FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("drafts-spec.log")
    }

    private static var failures = 0
    private static var checks = 0

    static func log(_ line: String) {
        print("DRAFTSPEC: \(line)")
        if let handle = try? FileHandle(forWritingTo: logURL) {
            handle.seekToEndOfFile()
            handle.write(Data("\(line)\n".utf8))
            try? handle.close()
        } else {
            try? Data("\(line)\n".utf8).write(to: logURL)
        }
    }

    /// One assertion. Records the mismatch rather than trapping, so one broken
    /// rule does not hide the others.
    private static func expect<T: Equatable>(_ actual: T, _ wanted: T, _ what: String) {
        checks += 1
        if actual != wanted {
            failures += 1
            log("FAIL \(what)\n       got:  \(actual)\n       want: \(wanted)")
        }
    }

    private static func expectTrue(_ actual: Bool, _ what: String) {
        expect(actual, true, what)
    }

    /// A store over a scratch file, so a spec run never touches the drafts of
    /// whoever is using this simulator.
    private static func scratch(_ name: String) -> DraftStore {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("drafts-spec-\(name).json")
        try? FileManager.default.removeItem(at: url)
        return DraftStore(fileURL: url)
    }

    // MARK: The run

    static func run() {
        try? FileManager.default.removeItem(at: logURL)
        failures = 0
        checks = 0
        log("drafts spec start")

        twoChatsKeepTwoDrafts()
        aCanvasDraftBelongsToItsSpace()
        theTwoThingsThatClearADraft()
        aDraftSurvivesRelaunch()
        referencesTravelWithTheirPrompt()
        theOldestDraftsAreEvictedPastTheCap()

        log(failures == 0
            ? "OK \(checks) checks, drafts hold"
            : "FAILED \(failures) of \(checks) checks")
        log("done")
    }

    // MARK: The rules

    /// The complaint in one check: text typed for one chat is still there
    /// after another chat has been visited, and is not the other chat's.
    private static func twoChatsKeepTwoDrafts() {
        let drafts = scratch("two-chats")
        drafts.setText("three\nlines\nhere", for: DraftStore.key(chat: "chat-a"))
        drafts.setText("something else", for: DraftStore.key(chat: "chat-b"))
        expect(drafts.text(for: "chat-a"), "three\nlines\nhere", "chat-a keeps its own")
        expect(drafts.text(for: "chat-b"), "something else", "chat-b keeps its own")
        expect(drafts.text(for: "chat-c"), "", "a chat never typed in has no draft")
    }

    /// The new-session canvas has no chat yet, so its text hangs off the
    /// space. Two spaces are two canvases — a prompt written for one repo must
    /// not turn up in another repo's composer, one tap from being sent there.
    private static func aCanvasDraftBelongsToItsSpace() {
        let drafts = scratch("canvas")
        drafts.setText("for the first repo", for: DraftStore.key(newSessionIn: "space-1"))
        expect(drafts.text(for: DraftStore.key(newSessionIn: "space-2")), "",
               "another space's canvas is another canvas")
        expect(DraftStore.key(newSessionIn: "space-1"), "new:space-1", "the canvas key")
        expect(DraftStore.key(chat: "chat-a"), "chat-a", "the chat key")
    }

    /// Sending it, or emptying it. Nothing else.
    private static func theTwoThingsThatClearADraft() {
        let drafts = scratch("clears")
        let key = DraftStore.key(chat: "chat-a")
        drafts.setText("typed", for: key)
        drafts.clear(key)                                   // what send() calls
        expect(drafts.text(for: key), "", "sending clears it")
        drafts.setText("typed again", for: key)
        drafts.setText("", for: key)                        // the box emptied
        expect(drafts.text(for: key), "", "emptying it clears it")
        expect(drafts.count, 0, "an empty draft is no draft, not a blank one")
    }

    /// Backgrounding and a cold launch: `flush()` is what the scene-phase hook
    /// runs, and a second store over the same file is what the next launch is.
    private static func aDraftSurvivesRelaunch() {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("drafts-spec-relaunch.json")
        try? FileManager.default.removeItem(at: url)
        let first = DraftStore(fileURL: url)
        first.setText("half a thought", for: DraftStore.key(chat: "chat-a"))
        first.flush()

        let relaunched = DraftStore(fileURL: url)
        expect(relaunched.text(for: "chat-a"), "half a thought", "the draft came back")

        // And a file the app never wrote does not stop it opening.
        let corrupt = FileManager.default.temporaryDirectory
            .appendingPathComponent("drafts-spec-corrupt.json")
        try? Data("{not json".utf8).write(to: corrupt)
        expect(DraftStore(fileURL: corrupt).count, 0, "a corrupt file starts empty")
    }

    /// The `@` references are part of the draft: a prompt that came back
    /// without them came back changed.
    private static func referencesTravelWithTheirPrompt() {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("drafts-spec-refs.json")
        try? FileManager.default.removeItem(at: url)
        let drafts = DraftStore(fileURL: url)
        let key = DraftStore.key(chat: "chat-a")
        let reference = ContextRef(path: "src/main.rs", kind: .file, checkoutId: "checkout-1")
        drafts.setText("look at @src/main.rs", for: key)
        drafts.setContext([reference], for: key)
        drafts.flush()

        let relaunched = DraftStore(fileURL: url)
        expect(relaunched.context(for: key), [reference], "the reference came back with it")
        expect(relaunched.context(for: key).first?.checkoutId, "checkout-1",
               "and the checkout it was picked from")
    }

    /// The cap is a backstop against a years-old install, not a policy anybody
    /// should feel: the newest drafts are the ones kept.
    private static func theOldestDraftsAreEvictedPastTheCap() {
        let drafts = scratch("cap")
        for i in 0..<(DraftStore.maxDrafts + 5) {
            drafts.setText("x", for: String(format: "chat-%04d", i))
        }
        expect(drafts.count, DraftStore.maxDrafts, "the store stops growing")
        // Same-second writes tie on `updatedAt` and break by key, so the five
        // that go are the five lowest — deterministic either way.
        expect(drafts.text(for: "chat-0000"), "", "the oldest went")
        expect(drafts.text(for: String(format: "chat-%04d", DraftStore.maxDrafts + 4)), "x",
               "the newest stayed")
    }
}
