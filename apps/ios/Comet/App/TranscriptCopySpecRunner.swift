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
        let longParagraphs = (1...18).map {
            "Long paragraph \($0) keeps this message beyond one phone viewport."
        }.joined(separator: "\n\n")
        let firstPart = """
        ## Complete result

        Keep `inline_code()` and the path below:

        - first bullet
          - nested at `/tmp/nested.swift`
        - second bullet at `/tmp/build.log`

        ```swift
        let answer = 42
        print(answer)
        ```

        | File | State |
        | --- | --- |
        | `TranscriptView.swift` | ready |

        > Copy the entire rendered message.

        \(longParagraphs)
        """
        let secondPart = """
        ### Continuation

        1. first ordered item
        2. second ordered item

        - [x] settled task
        - [ ] open task

        Final path: `\(tail)`
        """
        let expectedLongParagraphs = (1...18).map {
            "Long paragraph \($0) keeps this message beyond one phone viewport."
        }.joined(separator: "\n\n")
        let expected = """
        Complete result

        Keep inline_code() and the path below:

        • first bullet
          • nested at /tmp/nested.swift
        • second bullet at /tmp/build.log

        let answer = 42
        print(answer)

        File\tState
        TranscriptView.swift\tready

        > Copy the entire rendered message.

        \(expectedLongParagraphs)

        Continuation

        1. first ordered item
        2. second ordered item

        ☑ settled task
        ☐ open task

        Final path: \(tail)
        """
        var parsers: [String: IncrementalMarkdownParser] = [:]
        var completed: [String: CompletedParse] = [:]
        let entry = MessageEntry(
            id: "copy-spec", role: .assistant,
            parts: [
                .text(id: "first", text: firstPart),
                .tool(id: "tool", call: RenderToolCall(tag: "readFile", fields: ["path": tail]),
                      isError: false, resolved: true),
                .text(id: "second", text: secondPart),
            ],
            createdAt: 1, deviceId: "spec", status: .complete, continuationOf: nil
        )
        let rows = TranscriptRowBuilder.rows(entries: [entry], pendingSends: [],
                                               parsers: &parsers, completed: &completed)
        let textRows = rows.filter { $0.partKey != nil }
        let rowPayloads = textRows.compactMap(\.messageCopyText)
        let line: String
        if rows.count > 18,
           rowPayloads.count == textRows.count,
           rowPayloads.allSatisfy({ $0 == expected }) {
            line = "OK exact complete-message copy formatting (\(expected.count) characters)\ndone\n"
        } else {
            let actual = rowPayloads.first ?? "<no payload>"
            line = """
                FAIL exact copy mismatch; rows=\(rows.count), textRows=\(textRows.count), \
                payloads=\(rowPayloads.count)
                --- expected ---
                \(expected)
                --- actual ---
                \(actual)
                done

                """
        }
        try? Data(line.utf8).write(to: logURL)
        print("TRANSCRIPT-COPY-SPEC: \(line)")
    }
}
