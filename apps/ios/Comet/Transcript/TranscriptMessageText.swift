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
                let children: [(text: String, isList: Bool)] = item.children.compactMap { child in
                    guard let text = render(child, depth: depth + 1) else { return nil }
                    if case .list = child { return (text, true) }
                    return (text, false)
                }
                guard let first = children.first?.text else {
                    return "\(String(repeating: "  ", count: depth))\(marker)"
                }
                let continuation = String(repeating: "  ", count: depth + 1)
                let firstIndented = first.replacingOccurrences(of: "\n", with: "\n\(continuation)")
                var line = "\(String(repeating: "  ", count: depth))\(marker) \(firstIndented)"
                for child in children.dropFirst() {
                    if child.isList {
                        line += "\n\(child.text)"
                    } else {
                        let text = child.text.replacingOccurrences(
                            of: "\n", with: "\n\(continuation)"
                        )
                        line += "\n\(continuation)\(text)"
                    }
                }
                return line
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
