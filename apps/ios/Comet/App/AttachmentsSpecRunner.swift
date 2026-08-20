// The Swift half of the cross-language attachment-transport fixture (gh#535) —
// launch with `-attachments-spec`, or run `scripts/ios-attachments-spec.sh`,
// which does the whole loop.
//
// `Composer/Attachments.swift` re-implements the trailer rules that live in
// `comet_proto::view::attachments`, because no Rust runs on this device. Two
// implementations of "where does the prompt end and the machine trailer begin"
// is how one surface comes to show somebody their own message with the trailer
// in it — or to eat a line of their text. So the cases live outside both:
// the Rust module writes `Spec/attachments-spec.json` and asserts the
// checked-in file still matches it; this runner asserts the Swift functions
// against the same file. Whichever side moves is the side that fails.
//
// As with `SpecRunner`, only the cargo half runs in CI (this one needs a
// simulator) — so regenerating the fixture without running this leaves the
// phone quietly wrong about the rule that just changed.

import Foundation

@MainActor
enum AttachmentsSpecRunner {
    static var logURL: URL {
        FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("spec.log")
    }

    private static var failures = 0
    private static var checks = 0

    static func log(_ line: String) {
        print("ATTACH-SPEC: \(line)")
        if let handle = try? FileHandle(forWritingTo: logURL) {
            handle.seekToEndOfFile()
            handle.write(Data("\(line)\n".utf8))
            try? handle.close()
        } else {
            try? Data("\(line)\n".utf8).write(to: logURL)
        }
    }

    private static func expect<T: Equatable>(_ actual: T, _ wanted: T, _ what: String) {
        checks += 1
        if actual != wanted {
            failures += 1
            log("FAIL \(what)\n       got:  \(actual)\n       want: \(wanted)")
        }
    }

    // MARK: The fixture

    private struct Spec: Decodable {
        var render: [RenderCase]
        var parse: [ParseCase]
        var isImage: [ImageCase]
    }

    private struct RenderCase: Decodable {
        var name: String
        var text: String
        var paths: [String]
        var expect: String
    }

    private struct ParseCase: Decodable {
        var name: String
        var content: String
        var expect: Parsed
        var railText: String

        struct Parsed: Decodable {
            var text: String
            var attachments: [Ref]
        }

        struct Ref: Decodable, Equatable {
            var id: String
            var path: String
            var name: String
            var isImage: Bool
        }
    }

    private struct ImageCase: Decodable {
        var path: String
        var expect: Bool
    }

    static func run() {
        try? FileManager.default.removeItem(at: logURL)
        failures = 0
        checks = 0
        log("attachments spec start")

        guard let url = Bundle.main.url(forResource: "attachments-spec", withExtension: "json") else {
            log("FAIL attachments-spec.json is not in the app bundle")
            log("done 1 failure(s) of 1 check(s)")
            return
        }
        let spec: Spec
        do {
            spec = try JSONDecoder().decode(Spec.self, from: try Data(contentsOf: url))
        } catch {
            // A fixture this side cannot read is itself a drift report: the
            // Rust emitted a shape the Swift models no longer describe.
            log("FAIL cannot decode the fixture: \(error)")
            log("done 1 failure(s) of 1 check(s)")
            return
        }

        for c in spec.render {
            expect(withAttachments(text: c.text, paths: c.paths), c.expect,
                   "withAttachments — \(c.name)")
        }
        for c in spec.parse {
            let parsed = parseUserMessageAttachments(c.content)
            expect(parsed.text, c.expect.text, "parse — \(c.name): body")
            expect(parsed.attachments.map {
                ParseCase.Ref(id: $0.id, path: $0.path, name: $0.name, isImage: $0.isImage)
            }, c.expect.attachments, "parse — \(c.name): refs")
            expect(userMessageRailText(c.content), c.railText, "railText — \(c.name)")
        }
        for c in spec.isImage {
            expect(isImagePath(c.path), c.expect, "isImagePath(\(c.path))")
        }

        log(failures == 0
            ? "OK \(checks) checks, no drift"
            : "FAILED \(failures) of \(checks) checks")
        log("done")
    }
}
