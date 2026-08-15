// The Swift half of the cross-language stats fixture — launch with `-spec`
// (or run `scripts/ios-stats-spec.sh`, which does the whole loop).
//
// `StatsModels.swift` is a second implementation of rules that already exist in
// `comet_proto::view::stats`, because no Rust runs on this device. Two
// implementations of one rule is how a phone comes to disagree with a laptop
// about a number somebody is deciding on, and until gh#157 only the Rust half
// had tests.
//
// So the cases live outside both: `crates/proto/src/view/stats.rs` (mod spec)
// writes every rule's inputs and expected outputs to `Spec/stats-spec.json` and
// asserts the checked-in file still matches the Rust; this runner asserts the
// Swift functions against the same file. Whichever side moves is the side that
// fails — the Rust test on a rule change here, this runner on a rule change
// there.
//
// **This half is not in CI, and that asymmetry is the thing to know.** The Rust
// test runs on every push; this needs a simulator and runs when somebody runs
// it. So a rule changed in Rust fails the fixture guard, gets regenerated, and
// CI goes green with the phone still wrong — the guard is a prompt, not an
// enforcement. Whoever regenerates the fixture owes this run in the same
// change; the Rust panic message says so at the moment it matters.
//
// Why a launch-arg runner and not XCTest: this project has one target and one
// shared scheme, and a test target means editing `project.pbxproj` and
// `Comet.xcscheme` — the two files the operator keeps uncommitted local changes
// in. `-bench` and `-e2e` already established the headless-runner idiom here,
// and it runs in one `simctl` command. A test target would not close the CI gap
// either: what is missing is a macOS runner with a simulator.

import Foundation

@MainActor
enum SpecRunner {
    static var logURL: URL {
        FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("spec.log")
    }

    private static var failures = 0
    private static var checks = 0

    static func log(_ line: String) {
        print("SPEC: \(line)")
        if let handle = try? FileHandle(forWritingTo: logURL) {
            handle.seekToEndOfFile()
            handle.write(Data("\(line)\n".utf8))
            try? handle.close()
        } else {
            try? Data("\(line)\n".utf8).write(to: logURL)
        }
    }

    /// One assertion. Records the mismatch rather than trapping, so one broken
    /// rule does not hide the other nine.
    private static func expect<T: Equatable>(_ actual: T, _ wanted: T, _ what: String) {
        checks += 1
        if actual != wanted {
            failures += 1
            log("FAIL \(what)\n       got:  \(actual)\n       want: \(wanted)")
        }
    }

    // MARK: The fixture

    private struct Spec: Decodable {
        var rankedTop: [RankedCase]
        var rankedTokens: [TokenCase]
        var humanTokens: [TokenLabelCase]
        var humanMinutes: [MinuteCase]
        var percent: [PercentCase]
        var barFraction: [BarCase]
        var humanUsd: [UsdCase]
        var boardStats: [StatsCase]
    }

    private struct RankedCase: Decodable {
        var name: String
        var tally: [String: Int]
        var max: Int
        var expect: [Tally]
    }

    private struct TokenCase: Decodable {
        var name: String
        var tally: [String: TokenUsage]
        var max: Int
        var expect: [TokenTally]
    }

    private struct TokenLabelCase: Decodable {
        var tokens: UInt64
        var expect: String
    }

    private struct MinuteCase: Decodable {
        var minutes: Int64
        var expect: String
    }

    private struct PercentCase: Decodable {
        var rate: Double?
        var expect: String?
    }

    private struct BarCase: Decodable {
        var value: Int
        var peak: Int
        var expect: Double
    }

    private struct UsdCase: Decodable {
        var dollars: Double
        var expect: String
    }

    private struct StatsCase: Decodable {
        var name: String
        /// A real serialized `BoardStats` — decoding it here is half the point,
        /// since a field whose name skewed would reach the screen as a zero.
        var stats: BoardStats
        var expect: Expect

        struct Expect: Decodable {
            var windowLabel: String
            var isEmpty: Bool
            var hasTokens: Bool
            var completionPercent: String?
            var coveragePercent: String?
            var tokenTotal: UInt64
            var peakDispatches: Int
            var peakTokens: UInt64
            /// gh#182. The three spend states — no rates, rates that priced
            /// nothing, a real figure — are the ones a surface must not
            /// collapse into a zero.
            var hasSpend: Bool
            var spendLabel: String
            /// gh#426. Per-agent rows carry raw identity and estimates over
            /// the wire; these pin the phone's display derivations.
            var agentLabels: [String]
            var agentPriceLabels: [String?]
            /// gh#228. The landing bar: four bands whatever the window did,
            /// the two losses among them, and shares taken over what landed
            /// rather than over what was touched.
            var landingHeadline: String
            var landingSegments: [LandingSegment]
            var landingInFlightNote: String?
            /// gh#271. The other meter's gate: a window nothing reported has
            /// no share to draw, and `0 of 0` would read as no pressure
            /// rather than as no measurements.
            var contextReported: Bool
        }
    }

    // MARK: The run

    static func run() {
        try? FileManager.default.removeItem(at: logURL)
        failures = 0
        checks = 0
        log("stats spec start")

        guard let url = Bundle.main.url(forResource: "stats-spec", withExtension: "json") else {
            log("FAIL stats-spec.json is not in the app bundle")
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

        for c in spec.rankedTop {
            expect(rankedTop(c.tally, c.max), c.expect, "rankedTop — \(c.name)")
        }
        for c in spec.rankedTokens {
            expect(rankedTokens(c.tally, c.max), c.expect, "rankedTokens — \(c.name)")
        }
        for c in spec.humanTokens {
            expect(humanTokens(c.tokens), c.expect, "humanTokens(\(c.tokens))")
        }
        for c in spec.humanMinutes {
            expect(humanMinutes(c.minutes), c.expect, "humanMinutes(\(c.minutes))")
        }
        for c in spec.percent {
            let rate = c.rate.map { "\($0)" } ?? "nil"
            expect(percentLabel(c.rate), c.expect, "percentLabel(\(rate))")
        }
        for c in spec.barFraction {
            expect(barFraction(c.value, c.peak), c.expect, "barFraction(\(c.value), \(c.peak))")
        }
        for c in spec.humanUsd {
            expect(humanUsd(c.dollars), c.expect, "humanUsd(\(c.dollars))")
        }
        for c in spec.boardStats {
            let s = c.stats
            let what = "boardStats — \(c.name)"
            expect(s.windowLabel, c.expect.windowLabel, "\(what): windowLabel")
            expect(s.isEmpty, c.expect.isEmpty, "\(what): isEmpty")
            expect(s.hasTokens, c.expect.hasTokens, "\(what): hasTokens")
            expect(percentLabel(s.completionRate), c.expect.completionPercent,
                   "\(what): completion percent")
            expect(percentLabel(s.tokenCoverage), c.expect.coveragePercent,
                   "\(what): coverage percent")
            expect(s.tokens.total, c.expect.tokenTotal, "\(what): token total")
            expect(peakDispatches(s.daily), c.expect.peakDispatches, "\(what): peak dispatches")
            expect(peakTokens(s.dailyTokens), c.expect.peakTokens, "\(what): peak tokens")
            expect(s.hasSpend, c.expect.hasSpend, "\(what): hasSpend")
            expect(s.spendLabel, c.expect.spendLabel, "\(what): spend label")
            expect((s.agentUsage ?? []).map(\.label), c.expect.agentLabels,
                   "\(what): agent labels")
            expect((s.agentUsage ?? []).map(\.priceLabel), c.expect.agentPriceLabels,
                   "\(what): agent price labels")
            // gh#228: four bands, always, and the shares are of what landed.
            expect(s.landing.headline, c.expect.landingHeadline, "\(what): landing headline")
            expect(s.landing.segments, c.expect.landingSegments, "\(what): landing segments")
            expect(s.landing.inFlightNote, c.expect.landingInFlightNote,
                   "\(what): in-flight note")
            // gh#271: the context half is gated on having been measured.
            expect(s.contextReported, c.expect.contextReported,
                   "\(what): context reported")
            // 24 slots exist even before anything has run in them.
            expect(s.hourOfDay.count, 24, "\(what): hour slots")
        }

        log(failures == 0
            ? "OK \(checks) checks, no drift"
            : "FAILED \(failures) of \(checks) checks")
        log("done")
    }
}
