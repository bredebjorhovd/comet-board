import Foundation

@MainActor
enum ForkSpecRunner {
    private struct Case: Decodable {
        var checkout: ForkCheckout
        var context: ForkContext
        var checkoutTag: String
        var contextTag: String
        var truncation: TranscriptTruncation?
        var truncated: Bool?
        var truncationText: String?
    }

    static func run() {
        guard let url = Bundle.main.url(forResource: "fork-spec", withExtension: "json"),
              let data = try? Data(contentsOf: url),
              let cases = try? JSONDecoder().decode([Case].self, from: data) else {
            print("FORK-SPEC: FAIL cannot decode fork-spec.json")
            return
        }
        var failures = 0
        for item in cases {
            let lineage = ChatLineage(
                sourceChatId: "fixture", sourceMessageId: nil,
                checkout: item.checkout, context: item.context,
                carriedMessages: nil, truncated: item.truncated ?? false,
                truncation: item.truncation)
            if lineage.checkoutTag != item.checkoutTag
                || lineage.contextTag != item.contextTag
                || lineage.truncationExplanation != (item.truncationText ?? "") {
                failures += 1
            }
        }
        print(failures == 0
              ? "FORK-SPEC: OK \(cases.count) shared cases"
              : "FORK-SPEC: FAIL \(failures) of \(cases.count) cases")
    }
}
