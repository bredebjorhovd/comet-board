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
        var receipts: [ReceiptCase]
        var reviewable: [ReviewableCase]
        var runsTests: [CommandCase]
    }

    /// One receipt and the sentence it reads as (gh#365). The receipt is a real
    /// serialized `VerdictReceipt`, so a field the board renamed fails here
    /// rather than reaching the screen as a verdict nobody recorded.
    private struct ReceiptCase: Decodable {
        var name: String
        var receipt: VerdictReceipt
        var expect: String
    }

    private struct ReviewableCase: Decodable {
        var state: String
        var attempts: Int
        var expect: Bool
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
        let fixtureData: Data
        do {
            fixtureData = try Data(contentsOf: url)
            spec = try JSONDecoder().decode(Spec.self, from: fixtureData)
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

        // The navigation door is the shared Rust rule, not an iOS state list:
        // every row with an attempt remains reviewable, including a cancelled
        // attempt back at ready and history behind a working retry.
        for c in spec.reviewable {
            let row = TaskRow(id: "gh:o/r#1", identifier: "gh#1", title: "A task",
                              state: BoardState.parse(c.state), attempts: c.attempts)
            let what = "review door — \(c.state), attempts \(c.attempts)"
            expect(boardReviewable(row), c.expect, "\(what): shared rule")
            expect(boardShowsReviewDoor(row), c.expect, "\(what): detail-sheet presentation")
        }

        for c in spec.receipts {
            expect(reviewReceiptLine(c.receipt), c.expect, "receipt — \(c.name)")
        }

        for c in spec.runsTests {
            expect(reviewRunsTests(c.command), c.expect, "runsTests(\(c.command))")
        }

        // Required wire keys fail closed. These three are the dangerous ones:
        // defaulting any of them manufactures a clean remainder. The only two
        // top-level compatibility defaults remain decodable when absent.
        if let object = try? JSONSerialization.jsonObject(with: fixtureData) as? [String: Any],
           let reviews = object["reviews"] as? [[String: Any]],
           let first = reviews.first,
           let reviewObject = first["review"] as? [String: Any] {
            for key in ["claims", "unclaimed", "changed"] {
                var missing = reviewObject
                missing.removeValue(forKey: key)
                expect(decodesAttemptReview(missing), false, "wire: missing required \(key) fails")
            }
            for key in ["claims_error", "effects"] {
                var compatible = reviewObject
                compatible.removeValue(forKey: key)
                expect(decodesAttemptReview(compatible), true,
                       "wire: missing defaulted \(key) remains compatible")
            }
            var missingNullable = reviewObject
            missingNullable.removeValue(forKey: "claimed_at")
            expect(decodesAttemptReview(missingNullable), false,
                   "wire: a missing nullable claimed_at is version skew, not null")
        } else {
            expect(false, true, "wire: fixture exposes a review object for strictness checks")
        }

        if let first = spec.reviews.first {
            let loaded = LoadedAttemptReview(review: first.review, host: "host-that-answered")
            expect(loaded.target.taskId, first.review.taskId,
                   "RPC identity: target keeps the reviewed task")
            expect(loaded.target.attempt, first.review.attempt,
                   "RPC identity: target pins the reviewed attempt")
            expect(loaded.target.host, "host-that-answered",
                   "RPC identity: target keeps the host that answered")

            let read = reviewReadParams(taskId: first.review.taskId)
            expect(Set(read.keys), Set(["taskId"]), "RPC identity: exact read params")
            expect(read["taskId"] as? String, first.review.taskId,
                   "RPC identity: read task id")

            let verdict = reviewVerdictParams(loaded.target, kind: .changesRequested,
                                              comment: "Please fix it")
            expect(Set(verdict.keys), Set(["taskId", "attempt", "kind", "comment"]),
                   "RPC identity: exact verdict params")
            expect(verdict["taskId"] as? String, first.review.taskId,
                   "RPC identity: verdict task id")
            expect(verdict["attempt"] as? Int64, first.review.attempt,
                   "RPC identity: verdict attempt")
            expect(verdict["kind"] as? String, VerdictKind.changesRequested.rawValue,
                   "RPC identity: verdict kind")
            expect(verdict["comment"] as? String, "Please fix it",
                   "RPC identity: verdict comment")

            expect(reviewPresentation(nil), .loading,
                   "presentation: nil is the loading state")
            let noPayload = "This row has no attempt to review."
            expect(reviewPresentation(.failed(noPayload)),
                   .unavailable(title: "Nothing to review", message: noPayload),
                   "presentation: no payload is named, never a clean review")
            expect(reviewPresentation(.read(loaded)), .review(loaded),
                   "presentation: a loaded payload reaches the review card")

            expect(reviewContextLine(first.review, workspace: "comet-board"),
                   "gh#117 · PR #9 · comet-board",
                   "header: identifier, PR, and workspace")
            expect(reviewContextLine(first.review, workspace: nil),
                   "gh#117 · PR #9 · r",
                   "header: repository is the workspace fallback")
        }

        // The one rule the fixture cannot carry: `turn_pill` lives in
        // `comet_ui`, where a Rust test has no way to hand it over. Small, and
        // asserted here rather than left to nobody. `receipt_line` used to sit
        // beside it and now comes off the fixture — it moved into `comet_board`
        // with gh#365, because three surfaces print it.
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

    private static func decodesAttemptReview(_ object: [String: Any]) -> Bool {
        guard let data = try? JSONSerialization.data(withJSONObject: object) else { return false }
        return (try? JSONDecoder().decode(AttemptReview.self, from: data)) != nil
    }
}
