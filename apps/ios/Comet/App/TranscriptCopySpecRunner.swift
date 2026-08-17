// Focused simulator check for the whole-message pasteboard representation.
// It deliberately uses a message taller than a phone viewport: the formatter
// consumes the complete parsed model, independent of which transcript rows a
// LazyVStack currently has mounted.

import Foundation

@MainActor
enum TranscriptCopySpecRunner {
    static var logURL: URL {
        FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("transcript-copy-spec.log")
    }

    static func run() {
        let tail = "/Users/dev/.comet-native/worktrees/comet-board/final-tail.swift"
        let source = """
        ## Complete result

        Keep `inline_code()` and the path below:

        - first bullet
        - second bullet at `/tmp/build.log`

        ```swift
        let answer = 42
        print(answer)
        ```

        | File | State |
        | --- | --- |
        | `TranscriptView.swift` | ready |

        > Copy the entire rendered message.

        \(String(repeating: "A deliberately long paragraph keeps this message beyond one phone viewport.\n\n", count: 18))
        Final path: `\(tail)`
        """
        let actual = TranscriptMessageText.render(MarkdownParser.parse(source).map(\.block))
        var parsers: [String: IncrementalMarkdownParser] = [:]
        var completed: [String: CompletedParse] = [:]
        let entry = MessageEntry(
            id: "copy-spec", role: .assistant,
            parts: [.text(id: "text", text: source)],
            createdAt: 1, deviceId: "spec", status: .complete, continuationOf: nil
        )
        let rows = TranscriptRowBuilder.rows(entries: [entry], pendingSends: [],
                                               parsers: &parsers, completed: &completed)
        let rowPayloads = rows.compactMap(\.messageCopyText)
        let required = [
            "Complete result",
            "inline_code()",
            "• first bullet",
            "• second bullet at /tmp/build.log",
            "let answer = 42\nprint(answer)",
            "TranscriptView.swift\tready",
            "> Copy the entire rendered message.",
            tail,
        ]
        let missing = required.filter { !actual.contains($0) }
        let line: String
        if missing.isEmpty,
           actual.hasSuffix("Final path: \(tail)"),
           rows.count > 18,
           rowPayloads.count == rows.count,
           rowPayloads.allSatisfy({ $0 == actual }) {
            line = "OK complete-message copy formatting (\(actual.count) characters)\ndone\n"
        } else {
            line = "FAIL missing or truncated: \(missing); rows=\(rows.count), payloads=\(rowPayloads.count)\ntail: \(actual.suffix(120))\ndone\n"
        }
        try? Data(line.utf8).write(to: logURL)
        print("TRANSCRIPT-COPY-SPEC: \(line)")
    }
}
