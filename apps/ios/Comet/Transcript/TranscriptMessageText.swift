// Plain-text representation of the markdown the transcript renders. This is
// the whole-message pasteboard payload: it is built from the complete model,
// never from the visible LazyVStack rows, so virtualization cannot trim it.

import Foundation

enum TranscriptMessageText {
    static func render(_ blocks: [MDBlock]) -> String {
        blocks.compactMap { render($0, depth: 0) }
            .filter { !$0.isEmpty }
            .joined(separator: "\n\n")
    }

    private static func render(_ block: MDBlock, depth: Int) -> String? {
        switch block {
        case .paragraph(let runs), .heading(_, let runs):
            return inline(runs)
        case .codeBlock(_, let code):
            return code
        case .blockquote(let children):
            return render(children).split(separator: "\n", omittingEmptySubsequences: false)
                .map { "> \($0)" }
                .joined(separator: "\n")
        case .list(let orderedStart, let items):
            return items.enumerated().map { ix, item in
                let marker: String
                if let checked = item.checked {
                    marker = checked ? "☑" : "☐"
                } else if let start = orderedStart {
                    marker = "\(start + ix)."
                } else {
                    marker = "•"
                }
                let body = item.children.compactMap { render($0, depth: depth + 1) }
                    .joined(separator: "\n\n")
                let continuation = String(repeating: "  ", count: depth + 1)
                let indented = body.replacingOccurrences(of: "\n", with: "\n\(continuation)")
                return "\(String(repeating: "  ", count: depth))\(marker) \(indented)"
            }.joined(separator: "\n")
        case .table(let header, let rows, _):
            return ([header] + rows).map { row in
                row.map(inline).joined(separator: "\t")
            }.joined(separator: "\n")
        case .rule:
            // A rule is visual separation; the surrounding blank lines carry
            // that separation in plain text without inventing message content.
            return nil
        }
    }

    private static func inline(_ runs: [InlineRun]) -> String {
        runs.map(\.text).joined()
    }
}
