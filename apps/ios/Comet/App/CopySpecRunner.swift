// What the phone puts on the clipboard, asserted — launch with `-copy-spec`
// (or run `scripts/ios-copy-spec.sh`, which does the whole loop).
//
// gh#534: copy and paste barely existed on either surface, and during the
// 2026-08-19 incident the operator could not get the failing app's own error
// string out of it — diagnosis ran on paraphrases over SSH. The rule that came
// out of that is "any string the app can show, the user can put on the
// clipboard", and on this device the string has to be REBUILT: what the
// transcript holds is a parsed block model, and a `Text` cannot be asked what
// it is showing.
//
// So `TranscriptText` is a serializer, and a serializer is a thing that can be
// silently wrong — a fence that does not outlast the code inside it, a list
// whose second paragraph escapes its item, an error message that comes back
// with a label the reader never saw. This checks the cases where that happens.
//
// A launch-arg runner rather than XCTest for the reason `SpecRunner` gives at
// length: one target, one shared scheme, and a test target means editing the
// two files the operator keeps local changes in. Unlike the stats and review
// runners this has no Rust half to disagree with — the desktop copies rendered
// glyphs out of its own selection model and never serializes a block tree — so
// the cases here are the specification, not a fixture.

import Foundation

@MainActor
enum CopySpecRunner {
    static var logURL: URL {
        FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("copy-spec.log")
    }

    private static var failures = 0
    private static var checks = 0

    static func log(_ line: String) {
        print("COPYSPEC: \(line)")
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

    // MARK: The run

    static func run() {
        try? FileManager.default.removeItem(at: logURL)
        failures = 0
        checks = 0
        log("copy spec start")

        proseKeepsItsMarkers()
        aFenceOutlastsTheCodeInsideIt()
        aBacktickInInlineCodeWidensItsFence()
        listItemsKeepTheirContinuations()
        aTableComesBackAsATable()
        anErrorCopiesVerbatim()
        aToolGroupCarriesItsCommands()
        theTranscriptIsTheSourceNotAReparse()
        anEmptyTranscriptIsEmpty()
        aUserTurnCopiesWhatWasTypedNotItsAttachmentTrailer()

        log(failures == 0
            ? "OK \(checks) checks, no drift"
            : "FAILED \(failures) of \(checks) checks")
        log("done")
    }

    // MARK: The rules

    private static func inline(_ text: String, _ style: InlineStyle = .plain) -> InlineRun {
        InlineRun(text: text, style: style)
    }

    /// A copied paragraph is markdown, not stripped prose: bold stays bold and
    /// a path in inline code stays quotable. The parsed model is all there is —
    /// the source was never kept — so this is a reconstruction, and it has to
    /// be one that survives being pasted back into a markdown field.
    private static func proseKeepsItsMarkers() {
        let runs = [
            inline("Edited "),
            inline("crates/ui/src/transcript.rs", InlineStyle(code: true)),
            inline(" and it is "),
            inline("done", InlineStyle(bold: true)),
            inline(" — see "),
            inline("the issue", InlineStyle(link: "https://example.test/534")),
        ]
        expect(TranscriptText.markdown(block: .paragraph(runs)),
               "Edited `crates/ui/src/transcript.rs` and it is **done** — see [the issue](https://example.test/534)",
               "a paragraph round-trips its markers")
        // The rendered reading — what a character selection on either surface
        // yields — has none of them.
        expect(TranscriptText.plain(runs: runs),
               "Edited crates/ui/src/transcript.rs and it is done — see the issue",
               "the plain reading drops the markers")
        expect(TranscriptText.markdown(block: .heading(level: 3, [inline("Findings")])),
               "### Findings", "a heading keeps its level")
    }

    /// A reply that quotes a fenced block is the case that breaks a naive
    /// three-backtick fence: the paste comes back cut in half at the inner
    /// fence, with the rest of the reply rendered as prose.
    private static func aFenceOutlastsTheCodeInsideIt() {
        let nested = "```sh\necho hi\n```"
        expect(TranscriptText.markdown(block: .codeBlock(language: "md", code: nested)),
               "````md\n\(nested)\n````",
               "the fence widens past the one inside it")
        expect(TranscriptText.markdown(block: .codeBlock(language: nil, code: "let x = 1")),
               "```\nlet x = 1\n```",
               "no language, still a fence")
        // A blank line inside a listing is content — it survives (the desktop
        // has the same rule in markdown/selection.rs resolve_spans).
        expect(TranscriptText.markdown(block: .codeBlock(language: "rs", code: "a\n\nb")),
               "```rs\na\n\nb\n```",
               "a blank line inside a block survives")
    }

    /// An agent explaining markdown writes a code span that CONTAINS backticks,
    /// and a single-backtick fence around it produces something that does not
    /// parse back as code at all.
    private static func aBacktickInInlineCodeWidensItsFence() {
        // Content that both contains and ends with a backtick: the fence grows
        // to two, and CommonMark's space padding keeps them apart.
        expect(TranscriptText.markdown(block: .paragraph([
            inline("use "), inline("`code`", InlineStyle(code: true)),
        ])), "use `` `code` ``", "a span of backticks gets a longer, padded fence")
        // A backtick in the middle only needs the longer fence.
        expect(TranscriptText.markdown(block: .paragraph([
            inline("a`b", InlineStyle(code: true)),
        ])), "``a`b``", "an interior backtick needs no padding")
    }

    /// A list item that carries a second paragraph or a nested list has to keep
    /// it INSIDE the item — unindented continuations end the list, and the rest
    /// of the item pastes back as a sibling paragraph.
    private static func listItemsKeepTheirContinuations() {
        let items = [
            MDListItem(checked: nil, children: [
                .paragraph([inline("first")]),
                .paragraph([inline("still first")]),
            ]),
            MDListItem(checked: nil, children: [.paragraph([inline("second")])]),
        ]
        expect(TranscriptText.markdown(block: .list(orderedStart: nil, items: items)),
               "- first\n\n  still first\n- second",
               "a continuation stays under its bullet")
        let ordered = [
            MDListItem(checked: nil, children: [.paragraph([inline("one")])]),
            MDListItem(checked: nil, children: [.paragraph([inline("two")])]),
        ]
        expect(TranscriptText.markdown(block: .list(orderedStart: 3, items: ordered)),
               "3. one\n4. two", "an ordered list keeps its start")
        let tasks = [
            MDListItem(checked: true, children: [.paragraph([inline("done")])]),
            MDListItem(checked: false, children: [.paragraph([inline("todo")])]),
        ]
        expect(TranscriptText.markdown(block: .list(orderedStart: nil, items: tasks)),
               "- [x] done\n- [ ] todo", "checkboxes survive")
    }

    private static func aTableComesBackAsATable() {
        let header = [[inline("file")], [inline("changed")]]
        let rows = [[[inline("a.rs")], [inline("+3")]]]
        expect(TranscriptText.markdown(block: .table(header: header, rows: rows,
                                                     align: [.left, .right])),
               "| file | changed |\n| :--- | ---: |\n| a.rs | +3 |",
               "a table keeps its alignment row")
    }

    /// The acceptance line from the issue: "copy an error banner's text
    /// verbatim". Verbatim means no label — the string a person is about to
    /// paste into a search box is the one the app showed them, and the chip's
    /// red box is what said it was an error.
    private static func anErrorCopiesVerbatim() {
        let message = "workspace has 22/22 rooms; refusing to open another"
        expect(TranscriptText.text(for: .errorChip(message: message)), message,
               "an error chip copies its message and nothing else")
        // In a whole-transcript export the red box is gone, so the label is not.
        let entries = [entry(role: .assistant, parts: [.error(id: "e1", message: message)])]
        expect(TranscriptText.transcript(entries: entries),
               "## Agent\n\nError: \(message)",
               "the export labels what the box used to say")
    }

    private static func aToolGroupCarriesItsCommands() {
        let tools = [
            ToolItem(call: call("exec", ["command": "cargo test"]), isError: false, resolved: true),
            ToolItem(call: call("exec", ["command": "cargo clippy"]), isError: true, resolved: true),
        ]
        expect(TranscriptText.text(for: .toolGroup(tools: tools, autoOpen: false)),
               "Ran 2 commands · 1 failed\n- Run: cargo test\n- Run (failed): cargo clippy",
               "the summary is a count and the commands are the answer")
        expect(TranscriptText.text(for: .skillChip(name: "comet-board", args: "status",
                                                   isError: false, resolved: true)),
               "/comet-board status", "a skill copies as it is invoked")
    }

    /// The export reads the PARTS, not a re-parse of them: a text part already
    /// holds the markdown the agent wrote, and round-tripping it through the
    /// block model would be lossy and would cost a full re-parse of the session
    /// in the gap between tapping Share and the sheet arriving.
    private static func theTranscriptIsTheSourceNotAReparse() {
        // Deliberately awkward markdown — a setext heading and a hard-wrapped
        // paragraph, neither of which survives a block-model round trip.
        let source = "Findings\n========\n\nline one\nline two"
        let entries = [
            entry(role: .user, parts: [.text(id: "p0", text: "what broke?")]),
            entry(role: .assistant, parts: [
                .text(id: "p1", text: source),
                .tool(id: "t1", call: call("readFile", ["path": "/a/b/c.rs"]),
                      isError: false, resolved: true),
            ]),
        ]
        expect(TranscriptText.transcript(entries: entries),
               "## You\n\nwhat broke?\n\n## Agent\n\n\(source)\n\n- Read: b/c.rs",
               "the agent's own source comes out unchanged")
        // A send still in flight is on screen, so it is in the export.
        expect(TranscriptText.transcript(entries: entries,
                                         pendingSends: [(messageId: "m9", text: "and now?", at: 0)]),
               "## You\n\nwhat broke?\n\n## Agent\n\n\(source)\n\n- Read: b/c.rs\n\n## You\n\nand now?",
               "an in-flight send is part of the transcript")
    }

    /// A prompt with attachments carries a trailer of absolute paths on the
    /// HOST (gh#535). Copying a row, or exporting the session, must give back
    /// what the person typed — the paths are machine text, and on a phone they
    /// name a filesystem the phone does not have. When the attachments were the
    /// whole message there is no prompt, so the names stand in for it.
    private static func aUserTurnCopiesWhatWasTypedNotItsAttachmentTrailer() {
        let withPrompt = withAttachments(text: "what broke here?",
                                         paths: ["/data/uploads/ab-shot.png"])
        expect(TranscriptText.text(for: .user(text: withPrompt)), "what broke here?",
               "a user row copies the prompt, not the trailer")
        let attachmentsOnly = withAttachments(text: "",
                                              paths: ["/data/uploads/ab-spec.pdf",
                                                      "/data/uploads/cd-log.txt"])
        expect(TranscriptText.text(for: .user(text: attachmentsOnly)),
               "ab-spec.pdf, cd-log.txt",
               "an attachment-only send copies its file names, never nothing")
        expect(TranscriptText.transcript(entries: [
            entry(role: .user, parts: [.text(id: "p0", text: withPrompt)]),
        ]), "## You\n\nwhat broke here?",
               "the export is what was said, not the paths it was said with")
    }

    private static func anEmptyTranscriptIsEmpty() {
        expect(TranscriptText.transcript(entries: []), "", "nothing in, nothing out")
        // An entry whose only part is an empty string contributes no heading —
        // "## Agent" with nothing under it is a turn that did not happen.
        expect(TranscriptText.transcript(entries: [
            entry(role: .assistant, parts: [.text(id: "p0", text: "")])
        ]), "", "an empty part is not a turn")
    }

    // MARK: Fixtures

    private static func entry(role: MessageRole, parts: [MessagePart]) -> MessageEntry {
        MessageEntry(id: "m\(parts.count)-\(role)", role: role, parts: parts,
                     createdAt: 0, deviceId: "dev", status: .complete, continuationOf: nil)
    }

    private static func call(_ tag: String, _ fields: [String: String]) -> RenderToolCall {
        RenderToolCall(tag: tag, fields: fields.mapValues { AnyHashable($0) })
    }
}
