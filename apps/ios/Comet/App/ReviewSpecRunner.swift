// The Swift half of the cross-language review fixture (gh#256) — launch with
// `-review-spec`, or run `scripts/ios-review-spec.sh`, which does the whole
// loop.
//
// `ReviewModels.swift` is a second implementation of the reading in
// `comet_board::claims` and `comet_board::effects`, because no Rust runs on
// this device. Two implementations of one rule is how a phone comes to disagree
// with a laptop about whether an attempt is trustworthy — and the review screen
// is the worst place in the app for that, since its whole job is to be trusted
// about what a run did.
//
// So the cases live outside both: `crates/board/tests/ios_review_spec.rs`
// writes every case's inputs and expected outputs to `Spec/review-spec.json`
// and asserts the checked-in file still matches the Rust; this runner asserts
// the Swift against the same file. Whichever side moves is the side that fails.
//
// **This half is not in CI**, for the same reason `SpecRunner` is not: it needs
// a simulator. A rule changed in the Rust fails the fixture guard, gets
// regenerated, and CI goes green with the phone still wrong. Whoever
// regenerates owes this run in the same change.
//
// It is a launch-arg runner and not XCTest for the reason the stats one is:
// this project has one target and one shared scheme, and a test target means
// editing `project.pbxproj` and `Comet.xcscheme` — two files the operator keeps
// uncommitted local changes in.

import Foundation

@MainActor
enum ReviewSpecRunner {
    static var logURL: URL {
        FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("review-spec.log")
    }

    private static var failures = 0
    private static var checks = 0

    static func log(_ line: String) {
        print("REVIEW-SPEC: \(line)")
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
        var reviews: [ReviewCase]
        var runsTests: [CommandCase]
    }

    private struct ReviewCase: Decodable {
        var name: String
        /// A real serialized `AttemptReview` — decoding it here is half the
        /// point, since a key whose name skewed would reach the screen as a
        /// zero or an empty list.
        var review: AttemptReview
        var expect: Expect

        struct Expect: Decodable {
            var claimed: Bool
            var complete: Bool
            var verdict: VerdictCase
            var findings: [FindingCase]
            var effectChips: [ChipCase]
            var claims: [ClaimCase]
            var diffLabel: String
            var diffAdded: Int
            var diffRemoved: Int
            var counts: [String]
            var contractDelivering: String
            var contractPostedOnly: String
        }
    }

    private struct VerdictCase: Decodable, Equatable {
        var tone: String
        var text: String
    }

    private struct FindingCase: Decodable, Equatable {
        var kind: String
        var text: String
    }

    private struct ChipCase: Decodable, Equatable {
        var text: String
        var ground: String
    }

    private struct ClaimCase: Decodable {
        var mark: String
        var chips: [ChipCase]
    }

    private struct CommandCase: Decodable {
        var command: String
        var expect: Bool
    }

    // MARK: The run

    static func run() {
        try? FileManager.default.removeItem(at: logURL)
        failures = 0
        checks = 0
        log("review spec start")

        guard let url = Bundle.main.url(forResource: "review-spec", withExtension: "json") else {
            log("FAIL review-spec.json is not in the app bundle")
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

        for c in spec.reviews {
            let r = c.review
            let what = "review — \(c.name)"
            expect(r.claimed, c.expect.claimed, "\(what): claimed")
            expect(r.complete, c.expect.complete, "\(what): complete")

            let verdict = r.verdict()
            expect(VerdictCase(tone: verdict.tone.rawValue, text: verdict.text),
                   c.expect.verdict, "\(what): verdict")

            let findings = r.findings().map {
                FindingCase(kind: $0.kind.rawValue, text: $0.text)
            }
            expect(findings, c.expect.findings, "\(what): findings")

            let chips = r.effectChips().map {
                ChipCase(text: $0.text, ground: $0.ground.rawValue)
            }
            expect(chips, c.expect.effectChips, "\(what): effect chips")

            expect(r.claims.count, c.expect.claims.count, "\(what): claim count")
            for (ix, claim) in r.claims.enumerated() where ix < c.expect.claims.count {
                let wanted = c.expect.claims[ix]
                expect(r.claimMark(claim).rawValue, wanted.mark, "\(what): claim \(ix + 1) mark")
                let claimChips = r.claimChips(claim).map {
                    ChipCase(text: $0.text, ground: $0.ground.rawValue)
                }
                expect(claimChips, wanted.chips, "\(what): claim \(ix + 1) chips")
            }

            let totals = r.diffTotals
            expect(totals.label, c.expect.diffLabel, "\(what): diff label")
            expect(totals.added, c.expect.diffAdded, "\(what): diff added")
            expect(totals.removed, c.expect.diffRemoved, "\(what): diff removed")
            expect(r.changed.map(\.counts), c.expect.counts, "\(what): file counts")

            expect(reviewContractLine(r, delivering: true), c.expect.contractDelivering,
                   "\(what): contract line (delivering)")
            expect(reviewContractLine(r, delivering: false), c.expect.contractPostedOnly,
                   "\(what): contract line (posted only)")
        }

        for c in spec.runsTests {
            expect(reviewRunsTests(c.command), c.expect, "runsTests(\(c.command))")
        }

        // The two rules the fixture cannot carry: `turn_pill` and
        // `receipt_line` live in `comet_ui`, where a Rust test has no way to
        // hand them over. Small, and asserted here rather than left to nobody.
        expect(reviewTurnPill(state: "review", answered: false)?.label, "Waiting on you",
               "turn pill: a review waits on you")
        expect(reviewTurnPill(state: "blocked", answered: false)?.label, "Blocked on you",
               "turn pill: a question waits on you too")
        expect(reviewTurnPill(state: "review", answered: true)?.label, "Answered",
               "turn pill: this session's receipt wins over the board's state")
        expect(reviewTurnPill(state: "working", answered: false)?.label, nil,
               "turn pill: a running attempt is nobody's turn")
        expect(reviewTurnPill(state: "done", answered: false)?.label, nil,
               "turn pill: a settled row is nobody's turn")
        // Never the blocked hue, including for `blocked`: that one belongs to
        // the unclaimed set alone.
        expect(reviewTurnPill(state: "blocked", answered: false)?.status, .review,
               "turn pill: blocked wears the review hue, never the loud one")

        expect(reviewReceiptLine(VerdictReceipt(taskId: "t", posted: true, delivered: true)),
               "Posted on the pull request, and delivered into the chat once.",
               "receipt: posted and delivered")
        expect(reviewReceiptLine(VerdictReceipt(taskId: "t", posted: true, delivered: false,
                                                notDelivered: "chat chat-1 no longer holds the agent")),
               "Posted on the pull request. Nothing was delivered into the chat: "
               + "chat chat-1 no longer holds the agent.",
               "receipt: posted, and the author is gone")
        expect(reviewReceiptLine(VerdictReceipt(taskId: "t", posted: false, delivered: false)),
               "Already on the pull request.",
               "receipt: the idempotent path")

        // A bare approval interrupts nobody; anything with words does.
        expect(reviewWorthDelivering(.approve, ""), false, "worthDelivering: a bare approval")
        expect(reviewWorthDelivering(.approve, "nice"), true, "worthDelivering: an approval with words")
        expect(reviewWorthDelivering(.changesRequested, ""), true,
               "worthDelivering: changes requested, always")
        expect(VerdictKind.approve.needsComment, false, "needsComment: approve")
        expect(VerdictKind.changesRequested.needsComment, true, "needsComment: changes requested")
        expect(VerdictKind.comment.needsComment, true, "needsComment: comment")

        log(failures == 0
            ? "OK \(checks) checks, no drift"
            : "FAILED \(failures) of \(checks) checks")
        log("done")
    }
}
