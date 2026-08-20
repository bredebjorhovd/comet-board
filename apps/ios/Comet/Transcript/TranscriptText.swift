// Transcript → text, for the clipboard and the share sheet (gh#534).
//
// The rule the 2026-08-19 incident wrote: any string the app can show, the user
// can put on the clipboard. On the phone that needs a serializer, because what
// the transcript holds is a parsed block model — nothing keeps the source
// markdown around, and `Text` views cannot be asked what they are showing.
//
// What comes out is MARKDOWN, not stripped prose: it is the honest rendering of
// what a transcript is, it pastes legibly into Notes, and it pastes *correctly*
// into the two places a copied agent reply usually lands — a GitHub comment and
// another agent's prompt. Stripping the markers would be a lossy conversion
// chosen on the user's behalf.
//
// Pure functions over the row model, so `-copy-spec` can check them without a
// window (see SpecRunner).

import Foundation

enum TranscriptText {
    /// Typed so `String.split` resolves to the Character overload rather than
    /// the RegexComponent one.
    private static let newline: Character = "\n"

    // MARK: Inline runs

    /// Inline runs back to markdown. Code wins over emphasis (a code run cannot
    /// also be bold in the model), and a link wraps whatever its children were.
    static func markdown(runs: [InlineRun]) -> String {
        runs.map { run in
            var text = run.text
            if run.style.code {
                // A span containing a backtick needs a longer fence than the
                // run it delimits, and one that BEGINS or ENDS with a backtick
                // needs a space either side too, or the fence and the content
                // run together into a longer fence. Both are CommonMark's own
                // rules, and `` `a` `` is exactly the string an agent writes
                // when it is explaining markdown.
                let longest = longestBacktickRun(in: text)
                let fence = String(repeating: "`", count: longest + 1)
                let pad = (text.hasPrefix("`") || text.hasSuffix("`")) ? " " : ""
                text = "\(fence)\(pad)\(text)\(pad)\(fence)"
            } else {
                if run.style.bold { text = "**\(text)**" }
                if run.style.italic { text = "*\(text)*" }
                if run.style.strikethrough { text = "~~\(text)~~" }
            }
            if let link = run.style.link, !link.isEmpty {
                text = "[\(text)](\(link))"
            }
            return text
        }.joined()
    }

    /// The runs as they READ — no markers. The rendered-glyph text, which is
    /// what a character selection on either surface yields.
    static func plain(runs: [InlineRun]) -> String {
        runs.map(\.text).joined()
    }

    private static func longestBacktickRun(in text: String) -> Int {
        var longest = 0
        var current = 0
        for character in text {
            if character == "`" {
                current += 1
                longest = max(longest, current)
            } else {
                current = 0
            }
        }
        return longest
    }

    // MARK: Blocks

    static func markdown(block: MDBlock, indent: String = "") -> String {
        let body: String
        switch block {
        case .paragraph(let runs):
            body = markdown(runs: runs)

        case .heading(let level, let runs):
            body = String(repeating: "#", count: max(1, min(level, 6))) + " " + markdown(runs: runs)

        case .codeBlock(let language, let code):
            // The fence has to outlast any fence INSIDE the block, or a copied
            // reply that quotes a fenced block comes back cut in half.
            let fence = String(repeating: "`", count: max(3, longestFence(in: code) + 1))
            body = "\(fence)\(language ?? "")\n\(code)\n\(fence)"

        case .blockquote(let children):
            body = children
                .map { markdown(block: $0) }
                .joined(separator: "\n\n")
                .split(separator: newline, omittingEmptySubsequences: false)
                .map { $0.isEmpty ? ">" : "> \($0)" }
                .joined(separator: "\n")

        case .list(let orderedStart, let items):
            body = items.enumerated().map { ix, item in
                let marker: String
                if let checked = item.checked {
                    marker = checked ? "- [x] " : "- [ ] "
                } else if let start = orderedStart {
                    marker = "\(start + ix). "
                } else {
                    marker = "- "
                }
                // Continuation lines align under the marker, so a nested list
                // or a second paragraph stays inside its item.
                let pad = String(repeating: " ", count: marker.count)
                let content = item.children
                    .map { markdown(block: $0, indent: pad) }
                    .joined(separator: "\n\n")
                return marker + indented(content, by: pad, skippingFirst: true)
            }.joined(separator: "\n")

        case .table(let header, let rows, let align):
            var lines = ["| " + header.map { markdown(runs: $0) }.joined(separator: " | ") + " |"]
            lines.append("| " + (0..<header.count).map { column -> String in
                switch column < align.count ? align[column] : MDAlign.none {
                case .left: return ":---"
                case .center: return ":---:"
                case .right: return "---:"
                case .none: return "---"
                }
            }.joined(separator: " | ") + " |")
            for row in rows {
                lines.append("| " + row.map { markdown(runs: $0) }.joined(separator: " | ") + " |")
            }
            body = lines.joined(separator: "\n")

        case .rule:
            body = "---"
        }
        guard !indent.isEmpty else { return body }
        return indented(body, by: indent, skippingFirst: true)
    }

    /// Indent continuation lines, leaving blank ones blank — an indented empty
    /// line is trailing whitespace, which is invisible on screen and shows up
    /// in every diff of whatever the paste lands in.
    private static func indented(_ text: String, by pad: String, skippingFirst: Bool) -> String {
        let lines: [Substring] = text.split(separator: newline, omittingEmptySubsequences: false)
        return lines.enumerated().map { ix, line -> String in
            if skippingFirst, ix == 0 { return String(line) }
            return line.isEmpty ? "" : pad + line
        }.joined(separator: "\n")
    }

    private static func longestFence(in code: String) -> Int {
        let lines: [Substring] = code.split(separator: newline, omittingEmptySubsequences: false)
        var longest = 0
        for line in lines {
            longest = max(longest, line.prefix { $0 == "`" }.count)
        }
        return longest
    }

    // MARK: Rows

    /// One row's text — what "Copy" on that row puts on the clipboard.
    ///
    /// Tool groups carry every chip, not just the summary: "Ran 3 commands" is
    /// a count and the commands are the answer.
    ///
    /// An error chip copies its message and NOTHING else — verbatim is the
    /// whole point of copying an error (gh#534 acceptance), and a "Error: "
    /// prefix is a word the reader did not see in the string they are trying to
    /// search for. The whole-transcript export labels it, because there the
    /// sentence has lost the red box that said what it was.
    static func text(for kind: RowKind) -> String {
        switch kind {
        case .user(let text):
            // What the person wrote, not what the composer appended: a user row
            // carries its attachments as a machine trailer of absolute host
            // paths (gh#535), and copying those back is copying something
            // nobody typed. An attachment-only send has no prompt to copy, so
            // it copies the file names instead of an empty string.
            return userTurnText(text)
        case .markdown(let block, _):
            return markdown(block: block)
        case .toolGroup(let tools, _):
            let lines = tools.map { tool -> String in
                let detail = tool.call.chipDetail
                let head = "\(tool.call.chipLabel)\(tool.isError ? " (failed)" : "")"
                return detail.isEmpty ? "- \(head)" : "- \(head): \(detail)"
            }
            return ([toolGroupSummary(tools)] + lines).joined(separator: "\n")
        case .skillChip(let name, let args, let isError, _):
            let invocation = [("/" + name), args ?? ""]
                .filter { !$0.isEmpty }
                .joined(separator: " ")
            return isError ? "\(invocation) (failed)" : invocation
        case .inputChip(let header, let resolved):
            return resolved ? "Question: \(header)" : "Question: awaiting your answer"
        case .errorChip(let message):
            return message
        }
    }

    /// What the person wrote in a user turn: the visible prompt, or — when the
    /// attachments WERE the message — the file names, so an attachment-only
    /// send copies as something rather than as nothing.
    private static func userTurnText(_ text: String) -> String {
        let parsed = parseUserMessageAttachments(text)
        if !parsed.text.isEmpty { return parsed.text }
        return parsed.attachments.map(\.name).joined(separator: ", ")
    }

    /// A whole transcript, turn by turn. Speaker headings only where a turn
    /// starts, so a reply split across five rows reads as one answer.
    static func transcript(_ rows: [TranscriptRow]) -> String {
        var out: [String] = []
        for row in rows {
            let body = text(for: row.kind)
            if body.isEmpty { continue }
            if row.turnStart {
                if case .user = row.kind {
                    out.append("## You")
                } else {
                    out.append("## Agent")
                }
            }
            out.append(body)
        }
        return out.joined(separator: "\n\n")
    }

    // MARK: Entries

    /// The whole transcript straight off the ENTRIES, never the rows.
    ///
    /// A text part already holds the markdown the agent wrote — re-deriving it
    /// from parsed blocks would be a lossy round trip paid for with a full
    /// re-parse of every settled message, which on a long session is hundreds
    /// of milliseconds in the gap between tapping Share and the sheet arriving.
    /// This walks the parts and copies the source out.
    static func transcript(entries: [MessageEntry],
                           pendingSends: [(messageId: String, text: String, at: Int64)] = []) -> String {
        var out: [String] = []
        for entry in entries {
            var body: [String] = []
            for part in entry.parts {
                switch part {
                case .text(_, let text):
                    // A user turn's attachment trailer is machine text (gh#535):
                    // an export is for reading, and host-local absolute paths
                    // are not what the person said. The file names stand in when
                    // the attachments were the whole message.
                    let visible = entry.role == .user ? userTurnText(text) : text
                    if !visible.isEmpty { body.append(visible) }
                case .tool(_, let call, let isError, _):
                    let detail = call.chipDetail
                    let head = "\(call.chipLabel)\(isError ? " (failed)" : "")"
                    body.append(detail.isEmpty ? "- \(head)" : "- \(head): \(detail)")
                case .input(_, _, let questions, let resolved):
                    let header = questions.first?.header ?? "Question"
                    body.append(resolved ? "Question: \(header)" : "Question: awaiting your answer")
                case .error(_, let message):
                    body.append("Error: \(message)")
                }
            }
            guard !body.isEmpty else { continue }
            out.append(entry.role == .user ? "## You" : "## Agent")
            out.append(body.joined(separator: "\n\n"))
        }
        // A send still in flight is on screen, so it is in the export.
        let known = Set(entries.map(\.id))
        for pending in pendingSends where !known.contains(pending.messageId) {
            out.append("## You")
            out.append(pending.text)
        }
        return out.joined(separator: "\n\n")
    }
}
