// The review screen on the phone (gh#256) — what changed, not which lines.
//
// The board dispatched the brief, so only the board can put what you asked for
// next to what the agent says it did next to what actually changed. That is the
// one question GitHub cannot answer about a run, and it is the only question
// this screen asks. It is deliberately **not** a diff viewer, here least of all:
// a 393pt column is the worst place in the fleet to read a diff, and building
// one would be a worse GitHub that fits in a pocket.
//
// It is the desktop's `crates/ui/src/review.rs` in the phone's design language,
// and the order of the body is that module's order because the order is the
// argument:
//
// 1. **The verdict strip first**, in the one hue this screen is allowed to
//    shout in — the ramp's blocked hue, and nothing else on the page may wear
//    it.
// 2. **The effects row before the claims** (§gh#236). A reader who has already
//    been told a fluent story reads numbers underneath it as confirmation; the
//    same numbers read first are what the story then has to agree with.
// 3. **Then the claims**, each one carrying underneath it what the board found
//    in the files that claim's own anchors reached. `✓` is reserved for a claim
//    something the agent did not author stands behind; a claim nothing checks
//    says `no test covers this` and drops to a dot.
// 4. **Then what nobody claimed**, which is the product.
//
// A phone adds one idea the desktop card does not need: **the detail folds**.
// The desktop draws every claim's matched files and every effect's reason at
// once because it has a column to spend; here the sections carry the answer and
// open on a tap to say where it came from. Nothing is hidden that changes the
// reading — the glyph, the chips and the counts are always on screen — and what
// folds is the evidence behind them.
//
// The sentences are not this file's. `ReviewModels.swift` is the port of
// `comet_board::claims` and `comet_board::effects`; this decides only how they
// look and what a tap runs.

import SwiftUI

// MARK: - Numbers

/// The design's chip: a 22pt cap with a 5pt dot, and the 12% tint the two
/// status grounds wear. Layout numbers, so they live beside the layout.
private enum ReviewMetrics {
    static let chipHeight: CGFloat = 22
    static let chipPadding: CGFloat = 8
    static let chipDot: CGFloat = 5
    static let chipTint: Double = Theme.statusChipTint
    static let chipGap: CGFloat = 6
    /// The gutter between a status letter and a path, and between a path and
    /// its counts. One number, so a claim's matched files and the unclaimed set
    /// line up as one table even though different views draw them.
    static let fileGutter: CGFloat = 10
    /// The width the glyph column reserves, so every claim's text starts on the
    /// same vertical.
    static let glyphWidth: CGFloat = 12
}

// MARK: - The screen

enum ReviewLoad: Equatable {
    case read(LoadedAttemptReview)
    case failed(String)
}

/// The three things the screen can present. Factored from the SwiftUI body so
/// the no-payload state is checked by the simulator runner instead of existing
/// only as a screenshot path.
enum ReviewPresentation: Equatable {
    case loading
    case unavailable(title: String, message: String)
    case review(LoadedAttemptReview)
}

func reviewPresentation(_ load: ReviewLoad?) -> ReviewPresentation {
    switch load {
    case .none:
        return .loading
    case .failed(let message):
        return .unavailable(title: "Nothing to review", message: message)
    case .read(let loaded):
        return .review(loaded)
    }
}

struct ReviewScreen: View {
    @Environment(AppModel.self) private var model
    @Environment(\.openURL) private var openURL

    let taskId: String
    /// The board row carries the Comet workspace; the review RPC deliberately
    /// carries attempt facts only. Used for the reference's context line.
    let workspace: String?

    /// Nil while the read is still out; the rest is the answer, good or bad.
    @State private var loaded: ReviewLoad?
    @State private var comment = ""
    /// The verdict in flight, so the button that was pressed is the one that
    /// says so — and so a second tap on either cannot post twice.
    @State private var sending: VerdictKind?
    @State private var submitError: String?
    @State private var receipt: VerdictReceipt?
    /// Which sections are open. Folded by default: the answer is above the
    /// fold, and what opens is where the answer came from.
    @State private var openEffects = false
    @State private var openEvidence = false
    @State private var openClaims: Set<Int> = []

    var body: some View {
        Group {
            switch reviewPresentation(loaded) {
            case .loading:
                loading
            case .unavailable(let title, let message):
                unavailable(title: title, message: message)
            case .review(let loaded):
                card(loaded)
            }
        }
        .background(SheetStyle.panel.ignoresSafeArea())
        .navigationTitle("Review")
        .navigationBarTitleDisplayMode(.inline)
        .task(id: taskId) {
            loaded = await model.attemptReview(taskId: taskId)
        }
    }

    // MARK: The three states

    private var loading: some View {
        HStack(spacing: Theme.spaceSM) {
            ProgressView().controlSize(.small).tint(Theme.textMuted)
            Text("Reading the attempt…")
                .font(Theme.sans(Theme.textBody))
                .foregroundStyle(Theme.textFaint)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    /// A task with no attempt to read, a board that is not answering, a host on
    /// another version. Named rather than drawn as an empty review: a blank
    /// claims list is a *claim* about the attempt, and this is the absence of
    /// one.
    private func unavailable(title: String, message: String) -> some View {
        ContentUnavailableView(title,
                               systemImage: "text.magnifyingglass",
                               description: Text(message))
    }

    private func card(_ loaded: LoadedAttemptReview) -> some View {
        let review = loaded.review
        return VStack(spacing: 0) {
            header(review)
            Divider().overlay(Theme.border)
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    verdictStrip(review)
                    band(raised: true) { askedFor(review) }
                    band { effects(review) }
                    claims(review)
                    if !review.evidence.checks.isEmpty || review.evidence.commands > 0 {
                        band { evidence(review) }
                    }
                    unclaimed(review)
                    diffStrip(review)
                }
                .padding(.bottom, Theme.spaceLG)
            }
            .scrollEdgeEffectStyle(.soft, for: .top)
            if review.prUrl != nil {
                bar(loaded)
            }
        }
    }

    // MARK: Header

    private func header(_ review: AttemptReview) -> some View {
        HStack(alignment: .top, spacing: Theme.spaceSM) {
            VStack(alignment: .leading, spacing: 2) {
                Text(review.brief.title)
                    .font(Theme.sans(Theme.textTitle, weight: .semibold))
                    .foregroundStyle(Theme.text)
                    .lineLimit(2)
                    .fixedSize(horizontal: false, vertical: true)
                Text(reviewContextLine(review, workspace: workspace))
                    .font(Theme.sans(Theme.textDense))
                    .foregroundStyle(Theme.textSubtle)
                    .lineLimit(1)
            }
            Spacer(minLength: Theme.spaceXS)
            if let pill = reviewTurnPill(state: review.state, answered: receipt != nil) {
                Text(pill.label)
                    .font(Theme.sans(Theme.textDense))
                    .foregroundStyle(Theme.status(pill.status))
                    .padding(.horizontal, Theme.spaceSM)
                    .padding(.vertical, 1)
                    .background(Theme.status(pill.status).opacity(Theme.statusBadgeTint),
                                in: RoundedRectangle(cornerRadius: Theme.radiusChip))
            }
        }
        .padding(.horizontal, Theme.spaceLG)
        .padding(.vertical, Theme.spaceMD)
    }

    // MARK: The verdict

    /// The one line the screen leads with, and the only place the loud hue is
    /// spent — apart from the unclaimed block it is pointing at.
    private func verdictStrip(_ review: AttemptReview) -> some View {
        let verdict = review.verdict()
        return HStack(alignment: .top, spacing: Theme.spaceSM) {
            Text(verdict.tone.loud ? "!" : "·")
                .font(Theme.mono(Theme.textCaption))
                .foregroundStyle(toneColor(verdict.tone))
                .frame(width: ReviewMetrics.glyphWidth, alignment: .center)
            Text(verdict.text)
                .font(Theme.sans(Theme.textBody))
                .foregroundStyle(toneColor(verdict.tone))
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.horizontal, Theme.spaceLG)
        .padding(.vertical, Theme.spaceMD)
        .background(verdict.tone.loud ? Theme.danger.opacity(Theme.statusHeaderTint) : Color.clear)
        .overlay(alignment: .bottom) { hairline }
    }

    private func toneColor(_ tone: ReviewTone) -> Color {
        switch tone {
        case .alarm: return Theme.danger
        case .settled: return Theme.settled
        case .unknown: return Theme.textSubtle
        }
    }

    // MARK: Asked for

    private func askedFor(_ review: AttemptReview) -> some View {
        VStack(alignment: .leading, spacing: Theme.spaceXS) {
            sectionLabel("Asked for")
            // The issue, not the interpolated dispatch prompt: a reviewer who
            // cannot see what was asked cannot judge what was answered.
            Text(briefText(review))
                .font(Theme.sans(Theme.textBody))
                .foregroundStyle(Theme.textMuted)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
            if !review.brief.url.isEmpty, let url = URL(string: review.brief.url) {
                Button("Open the issue") { openURL(url) }
                    .font(Theme.sans(Theme.textCaption))
                    .foregroundStyle(Theme.accent)
                    .buttonStyle(.plain)
            }
        }
    }

    /// An issue with no description and an issue nobody could read are
    /// different answers, and the band says which.
    private func briefText(_ review: AttemptReview) -> String {
        guard let body = review.brief.body?
            .trimmingCharacters(in: .whitespacesAndNewlines), !body.isEmpty else {
            return "This issue has no description — the title above is the whole brief."
        }
        return body
    }

    // MARK: Effects

    private func effects(_ review: AttemptReview) -> some View {
        let chips = review.effectChips()
        return VStack(alignment: .leading, spacing: Theme.spaceSM) {
            foldHeader("Effects", open: openEffects) { openEffects.toggle() }
            chipRow(chips)
            if openEffects {
                VStack(alignment: .leading, spacing: Theme.spaceXS) {
                    ForEach(Array(effectNotes(review).enumerated()), id: \.offset) { _, line in
                        noteRow(line)
                    }
                }
                .padding(.top, 2)
            }
        }
    }

    /// Where each chip came from, for the reader who does not take a chip's
    /// word for it. Only the chips that HAVE a source say anything: a row of
    /// five explanations for five one-line facts would be the screen shouting
    /// again.
    private func effectNotes(_ review: AttemptReview) -> [String] {
        var out: [String] = []
        let effects = review.effects
        guard effects.read else {
            return ["The board never read this branch — its checkout was gone before "
                    + "anything looked, so none of the five questions has an answer."]
        }
        if let before = effects.testsBefore, let after = effects.testsAfter {
            out.append("Test declarations in the whole tree: \(before) before the branch, "
                       + "\(after) after.")
        }
        switch reviewTestRun(review.evidence) {
        case .passing:
            out.append("Every test command in the run journal passed at least once.")
        case .failing(let n):
            out.append("\(reviewCount(n, "test command", "test commands")) ran and never "
                       + "once passed.")
        case .neverRun:
            out.append("No test command ran, so the counts above are declarations and "
                       + "not results.")
        }
        if case .unknown(let reason) = effects.publicAPI {
            out.append("Public API unknown: \(reason).")
        }
        if !effects.depsKnown {
            out.append("A changed manifest could not be read on both sides, so a new "
                       + "dependency cannot be ruled out.")
        } else if !effects.depsAdded.isEmpty {
            out.append("Added: \(effects.depsAdded.joined(separator: ", ")).")
        }
        return out
    }

    // MARK: Claims

    private func claims(_ review: AttemptReview) -> some View {
        VStack(alignment: .leading, spacing: Theme.spaceSM) {
            HStack(spacing: Theme.spaceSM) {
                Text("What it says it did")
                    .font(Theme.sans(Theme.textBody, weight: .semibold))
                    .foregroundStyle(Theme.text)
                Text(claimsAside(review))
                    .font(Theme.sans(Theme.textDense))
                    .foregroundStyle(Theme.textSubtle)
                Spacer(minLength: 0)
            }
            if review.claims.isEmpty {
                if let text = emptyClaimsText(review) {
                    Text(text)
                        .font(Theme.sans(Theme.textBody))
                        .foregroundStyle(Theme.textSubtle)
                        .fixedSize(horizontal: false, vertical: true)
                }
            } else {
                ForEach(Array(review.claims.enumerated()), id: \.offset) { index, claim in
                    claimCard(review, claim, index: index)
                }
            }
        }
        .padding(.horizontal, Theme.spaceLG)
        .padding(.vertical, Theme.spaceMD)
        .overlay(alignment: .bottom) { hairline }
    }

    private func claimsAside(_ review: AttemptReview) -> String {
        review.claims.isEmpty
            ? "none"
            : reviewCount(review.claims.count, "claim", "claims")
    }

    /// Never asked is not claimed nothing (§gh#183), and a badly-answered
    /// contract is neither — a screen that drew them alike would be flattening
    /// the one distinction this whole design exists to keep.
    ///
    /// Nil when the verdict strip has already said it in those words. The
    /// strip leads with the loudest finding, and two of the three sentences
    /// here ARE that finding; repeating one under a heading four inches lower
    /// reads as a second, different problem.
    private func emptyClaimsText(_ review: AttemptReview) -> String? {
        switch review.findings().first?.kind {
        case .neverClaimed, .malformedClaims: return nil
        default: break
        }
        if let error = review.claimsError {
            return "This attempt wrote a claims block the board could not read: "
                + reviewFirstLine(error)
        }
        if review.undispatched {
            // Nobody was asked, so nobody ignored the ask (§gh#344).
            return "Nothing dispatched this pull request, so nobody was ever told "
                + "the claim contract."
        }
        return review.claimed
            ? "This attempt answered the contract and claimed nothing."
            : "This attempt never answered the claim contract."
    }

    private func claimCard(_ review: AttemptReview, _ claim: ClaimView, index: Int) -> some View {
        let mark = review.claimMark(claim)
        let chips = review.claimChips(claim)
        let open = openClaims.contains(index)
        let matched = claim.matched.compactMap { path in
            review.changed.first { $0.path == path }
        }
        return Button {
            if open { openClaims.remove(index) } else { openClaims.insert(index) }
        } label: {
            HStack(alignment: .top, spacing: Theme.spaceSM) {
                Text(mark.glyph)
                    .font(Theme.mono(Theme.textCaption))
                    .foregroundStyle(claimColor(mark))
                    .frame(width: ReviewMetrics.glyphWidth, alignment: .center)
                    .padding(.top, 2)
                VStack(alignment: .leading, spacing: Theme.spaceSM) {
                    Text(claim.text)
                        .font(Theme.sans(Theme.textBody))
                        .foregroundStyle(mark == .bare ? Theme.textMuted : Theme.text)
                        .fixedSize(horizontal: false, vertical: true)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    if !chips.isEmpty { chipRow(chips) }
                    if open {
                        // The evidence, spelled exactly as the unclaimed set
                        // spells it — the two read as one table.
                        VStack(alignment: .leading, spacing: 2) {
                            ForEach(matched, id: \.path) { file in
                                fileRow(file, loud: false)
                            }
                            ForEach(claim.unmatched, id: \.self) { anchor in
                                HStack(spacing: ReviewMetrics.fileGutter) {
                                    Text("?")
                                        .font(Theme.mono(Theme.textCaption))
                                        .foregroundStyle(Theme.danger)
                                        .frame(width: 9, alignment: .leading)
                                    Text(anchor)
                                        .font(Theme.mono(Theme.textCaption))
                                        .foregroundStyle(Theme.textMuted)
                                        .lineLimit(1)
                                        .truncationMode(.middle)
                                    Spacer(minLength: 0)
                                    Text("nothing changed")
                                        .font(Theme.sans(Theme.textCaption))
                                        .foregroundStyle(Theme.textFaint)
                                }
                            }
                            if matched.isEmpty && claim.unmatched.isEmpty {
                                Text("This claim named no anchor the diff knows about.")
                                    .font(Theme.sans(Theme.textCaption))
                                    .foregroundStyle(Theme.textFaint)
                            }
                        }
                        .padding(.top, 2)
                    }
                }
            }
            .padding(.horizontal, Theme.spaceMD)
            .padding(.vertical, Theme.spaceSM + 2)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .overlay(RoundedRectangle(cornerRadius: Theme.radiusCard)
            .strokeBorder(mark == .contradicted ? Theme.danger.opacity(Theme.statusBorderTint) : Theme.border,
                          lineWidth: 1))
    }

    private func claimColor(_ mark: ClaimMark) -> Color {
        switch mark {
        case .contradicted: return Theme.danger
        case .corroborated: return Theme.settled
        case .bare: return Theme.textSubtle
        }
    }

    // MARK: Evidence

    /// What ran, off the run journal rather than off a claim. The totals are
    /// the denominator that stops an empty list reading as a quiet run.
    private func evidence(_ review: AttemptReview) -> some View {
        let checks = review.evidence.checks
        return VStack(alignment: .leading, spacing: Theme.spaceSM) {
            foldHeader("What ran", open: openEvidence, aside: evidenceAside(review)) {
                openEvidence.toggle()
            }
            if openEvidence {
                if checks.isEmpty {
                    Text("Nothing in the journal verifies anything.")
                        .font(Theme.sans(Theme.textCaption))
                        .foregroundStyle(Theme.textSubtle)
                } else {
                    VStack(alignment: .leading, spacing: Theme.spaceXS) {
                        ForEach(checks, id: \.command) { check in
                            HStack(spacing: ReviewMetrics.fileGutter) {
                                Text(check.command)
                                    .font(Theme.mono(Theme.textCaption))
                                    .foregroundStyle(check.everPassed
                                                     ? Theme.textMuted : Theme.danger)
                                    .lineLimit(1)
                                    .truncationMode(.middle)
                                Spacer(minLength: 0)
                                Text(checkCount(check))
                                    .font(Theme.sans(Theme.textCaption))
                                    .foregroundStyle(Theme.textFaint)
                            }
                        }
                        if review.evidence.truncated {
                            Text("The list was cut short — this run used more distinct "
                                 + "check commands than the board keeps.")
                                .font(Theme.sans(Theme.textCaption))
                                .foregroundStyle(Theme.textFaint)
                        }
                    }
                }
            }
        }
    }

    private func evidenceAside(_ review: AttemptReview) -> String {
        let commands = reviewCount(review.evidence.commands, "command", "commands")
        guard review.evidence.checked else { return "\(commands), none of them a check" }
        return "\(reviewCount(review.evidence.checks.count, "check", "checks")) of \(commands)"
    }

    private func checkCount(_ check: RunCheck) -> String {
        let runs = "×\(check.runs)"
        return check.failed > 0 ? "\(runs) · \(check.failed) failed" : runs
    }

    // MARK: What nobody claimed

    /// The product, and the only block on this screen allowed to shout.
    @ViewBuilder
    private func unclaimed(_ review: AttemptReview) -> some View {
        // The findings that belong beside the remainder rather than in the
        // strip — minus whichever one the strip is already leading with. The
        // desktop can afford to say the loudest finding twice; a 393pt column
        // reads the repeat as a second, different problem.
        let findings = review.findings()
        let noteKinds: [ReviewFindingKind] = [.uncommitted, .neverClaimed, .malformedClaims]
        let notes = findings.filter { $0 != findings.first && noteKinds.contains($0.kind) }
        VStack(alignment: .leading, spacing: Theme.spaceSM) {
            if let reason = review.diff.unavailableReason {
                sectionLabel("Unclaimed")
                Text("Unknown — there is no diff to check against: \(reason)")
                    .font(Theme.sans(Theme.textBody))
                    .foregroundStyle(Theme.textSubtle)
                    .fixedSize(horizontal: false, vertical: true)
            } else if review.unclaimed.isEmpty {
                sectionLabel("Unclaimed")
                if review.claimed {
                    Text("Nothing — every changed file is accounted for.")
                        .font(Theme.sans(Theme.textBody))
                        .foregroundStyle(Theme.settled)
                }
                notesList(notes)
            } else {
                unclaimedBlock(review)
                notesList(notes)
            }
            if case .recorded = review.diff {
                Text("From the diff the board recorded while the run was live; "
                     + "the checkout is gone.")
                    .font(Theme.sans(Theme.textCaption))
                    .foregroundStyle(Theme.textFaint)
                    .fixedSize(horizontal: false, vertical: true)
            }
            // Where the numbers came from when nothing local ever held this
            // branch (§gh#344): a count is only as good as its provenance.
            if case .pullRequest = review.diff {
                Text("From GitHub's file list for the pull request; "
                     + "nothing ran here to read a checkout of.")
                    .font(Theme.sans(Theme.textCaption))
                    .foregroundStyle(Theme.textFaint)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(.horizontal, Theme.spaceLG)
        .padding(.vertical, Theme.spaceMD)
    }

    private func unclaimedBlock(_ review: AttemptReview) -> some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: Theme.spaceSM) {
                Image(systemName: "exclamationmark.triangle")
                    .font(.system(size: 13))
                    .foregroundStyle(Theme.danger)
                Text("\(reviewCount(review.unclaimed.count, "change", "changes")) "
                     + "no claim accounts for")
                    .font(Theme.sans(Theme.textBody, weight: .semibold))
                    .foregroundStyle(Theme.danger)
                Spacer(minLength: 0)
            }
            .padding(.horizontal, Theme.spaceMD)
            .padding(.vertical, Theme.spaceSM + 1)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Theme.danger.opacity(Theme.statusHeaderTint))
            // ios.md D4.2: the header's own divider, at the canvas's 22% —
            // heavier than `--line`, because it separates the warning from
            // the evidence rather than one piece of evidence from the next.
            Rectangle()
                .fill(Theme.danger.opacity(Theme.statusDividerTint))
                .frame(height: 1)
            ForEach(Array(review.unclaimed.enumerated()), id: \.offset) { index, file in
                if index > 0 {
                    Rectangle().fill(Theme.border).frame(height: 1)
                }
                fileRow(file, loud: true)
                    .padding(.horizontal, Theme.spaceMD)
                    .padding(.vertical, Theme.spaceSM + 1)
            }
        }
        .clipShape(RoundedRectangle(cornerRadius: Theme.radiusCard))
        .overlay(RoundedRectangle(cornerRadius: Theme.radiusCard)
            .strokeBorder(Theme.danger.opacity(Theme.statusBorderTint), lineWidth: 1))
    }

    @ViewBuilder
    private func notesList(_ notes: [ReviewFinding]) -> some View {
        ForEach(Array(notes.enumerated()), id: \.offset) { _, finding in
            HStack(alignment: .top, spacing: ReviewMetrics.chipGap) {
                Text(finding.kind.tone.loud ? "!" : "·")
                    .font(Theme.mono(Theme.textCaption))
                    .foregroundStyle(finding.kind.tone.loud ? Theme.danger : Theme.textFaint)
                    .frame(width: ReviewMetrics.glyphWidth, alignment: .center)
                Text(finding.text)
                    .font(Theme.sans(Theme.textCaption))
                    .foregroundStyle(toneColor(finding.kind.tone))
                    .fixedSize(horizontal: false, vertical: true)
                Spacer(minLength: 0)
            }
        }
    }

    // MARK: How much moved

    /// The strip that closes the body (§gh#238): what moved, in numbers.
    ///
    /// The desktop's version carries a `Read the diff` chip beside these
    /// counts. This one does not, and it is not an omission: the chip hands the
    /// reader to a diff viewer, and there is none here — a 393pt column is the
    /// worst surface in the fleet for reading one. The counts are the fact; the
    /// pull request in the header is the way out.
    @ViewBuilder
    private func diffStrip(_ review: AttemptReview) -> some View {
        if !review.changed.isEmpty {
            let totals = review.diffTotals
            HStack(spacing: Theme.spaceSM) {
                Text(totals.label)
                    .font(Theme.sans(Theme.textCaption))
                    .foregroundStyle(Theme.textSubtle)
                Text("+\(totals.added)")
                    .font(Theme.mono(Theme.textCaption))
                    .foregroundStyle(Theme.textMuted)
                // The one carve-out on this screen: the hue belongs to the
                // minus sign, not to the page. Every diff in this app and every
                // other one paints it red, and a grey one would be the only one
                // that is not.
                Text("−\(totals.removed)")
                    .font(Theme.mono(Theme.textCaption))
                    .foregroundStyle(Theme.danger)
                Spacer(minLength: 0)
            }
            .padding(.horizontal, Theme.spaceLG)
            .padding(.vertical, Theme.spaceMD)
            .overlay(alignment: .top) { hairline }
        }
    }

    // MARK: The bar

    /// Whose turn it is, answered (§gh#239). Two buttons and not the desktop's
    /// three: a bare `Comment` is a conversation, and the chat it belongs in is
    /// one tap away on the board. What the phone is for is the decision.
    private func bar(_ loaded: LoadedAttemptReview) -> some View {
        let review = loaded.review
        let trimmed = comment.trimmingCharacters(in: .whitespacesAndNewlines)
        // Which verdict the sentence is about: with an empty box the only
        // button that can be pressed is Approve, and a bare approval is not
        // worth interrupting an agent with. Asked of the shared rule rather
        // than restated, so the bar cannot promise a delivery the board would
        // then decline to make.
        let delivering = reviewWorthDelivering(
            trimmed.isEmpty ? .approve : .changesRequested, trimmed)
        return VStack(alignment: .leading, spacing: Theme.spaceSM) {
            TextField("Anything to say about it?", text: $comment, axis: .vertical)
                .lineLimit(1...4)
                .font(Theme.sans(Theme.textBody))
                .foregroundStyle(Theme.text)
                .textFieldStyle(.plain)
                .padding(.horizontal, Theme.spaceMD)
                .padding(.vertical, Theme.spaceSM + 1)
                .background(Theme.bg, in: RoundedRectangle(cornerRadius: Theme.radiusRow))
                .overlay(RoundedRectangle(cornerRadius: Theme.radiusRow)
                    .strokeBorder(Theme.borderStrong, lineWidth: 1))
                .disabled(sending != nil || receipt != nil)
            if let receipt {
                Text(reviewReceiptLine(receipt))
                    .font(Theme.sans(Theme.textCaption))
                    .foregroundStyle(receipt.delivered ? Theme.settled : Theme.textMuted)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
            } else {
                // The promise, on its own line and above the buttons. Beside
                // them — where the design file draws it — it is the first thing
                // a 393pt column truncates, and a promise that ends in an
                // ellipsis is not one.
                Text(reviewContractLine(review, delivering: delivering))
                    .font(Theme.sans(Theme.textCaption))
                    .foregroundStyle(Theme.textFaint)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
                HStack(spacing: Theme.spaceSM) {
                    Spacer(minLength: 0)
                    verdictButton(.approve, target: loaded.target, filled: false)
                    verdictButton(.changesRequested, target: loaded.target, filled: true)
                }
            }
            if let submitError {
                Text(submitError)
                    .font(Theme.sans(Theme.textCaption))
                    .foregroundStyle(Theme.warning)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(.horizontal, Theme.spaceLG)
        .padding(.vertical, Theme.spaceMD)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Theme.surfaceRaised)
        .overlay(alignment: .top) { hairline }
    }

    private func verdictButton(_ kind: VerdictKind, target: ReviewTarget,
                               filled: Bool) -> some View {
        // GitHub refuses an empty `REQUEST_CHANGES` and it is right to: a
        // verdict with nothing in it tells the agent to change something
        // unnamed. So the button that needs prose is dark until there is some.
        let ready = !(kind.needsComment
            && comment.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
        return Button {
            send(kind, target: target)
        } label: {
            Text(sending == kind ? "Sending…" : kind.label)
                .font(Theme.sans(Theme.textBody, weight: filled ? .medium : .regular))
                .foregroundStyle(buttonForeground(filled: filled, ready: ready))
                .padding(.horizontal, Theme.spaceMD)
                .frame(height: 32)
                .background(buttonBackground(filled: filled, ready: ready),
                            in: RoundedRectangle(cornerRadius: Theme.radiusRow))
        }
        .buttonStyle(.plain)
        .disabled(!ready || sending != nil)
    }

    private func buttonForeground(filled: Bool, ready: Bool) -> Color {
        if !ready { return Theme.textFaint }
        return filled ? Theme.bg : Theme.settled
    }

    private func buttonBackground(filled: Bool, ready: Bool) -> Color {
        guard filled else { return .clear }
        return ready ? Theme.text : Theme.chip
    }

    private func send(_ kind: VerdictKind, target: ReviewTarget) {
        guard sending == nil, receipt == nil else { return }
        sending = kind
        submitError = nil
        let text = comment.trimmingCharacters(in: .whitespacesAndNewlines)
        Task {
            switch await model.submitVerdict(target: target, kind: kind, comment: text) {
            case .sent(let sent):
                receipt = sent
            case .failed(let message):
                submitError = message
            }
            sending = nil
        }
    }

    // MARK: Shared pieces

    /// A band: one section, padded, with the hairline that separates it from
    /// the next.
    ///
    /// `raised` is the canvas's `--raised` fill, and exactly one band on this
    /// screen wears it — "Asked for" (ios.md D2.1). It is the one thing here
    /// the agent did not write, so it reads as a quotation rather than as
    /// another of the screen's own sections.
    private func band<Content: View>(raised: Bool = false,
                                     @ViewBuilder _ content: () -> Content) -> some View {
        content()
            .padding(.horizontal, Theme.spaceLG)
            .padding(.vertical, Theme.spaceMD)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(raised ? Theme.surfaceRaised : Color.clear)
            .overlay(alignment: .bottom) { hairline }
    }

    private var hairline: some View {
        Rectangle().fill(Theme.border).frame(height: 1)
    }

    private func sectionLabel(_ text: String) -> some View {
        Text(text)
            .font(Theme.sans(Theme.textDense, weight: .semibold))
            .foregroundStyle(Theme.textSubtle)
    }

    private func foldHeader(_ title: String, open: Bool, aside: String? = nil,
                            toggle: @escaping () -> Void) -> some View {
        Button(action: toggle) {
            HStack(spacing: Theme.spaceSM) {
                sectionLabel(title)
                if let aside {
                    Text(aside)
                        .font(Theme.sans(Theme.textCaption))
                        .foregroundStyle(Theme.textFaint)
                        .lineLimit(1)
                }
                Spacer(minLength: 0)
                Image(systemName: "chevron.down")
                    .font(.system(size: 10, weight: .semibold))
                    .foregroundStyle(Theme.textFaint)
                    .rotationEffect(.degrees(open ? 0 : -90))
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .motionAnimation(Motion.collapse, value: open)
    }

    private func noteRow(_ text: String) -> some View {
        HStack(alignment: .top, spacing: ReviewMetrics.chipGap) {
            Text("·")
                .font(Theme.mono(Theme.textCaption))
                .foregroundStyle(Theme.textFaint)
                .frame(width: ReviewMetrics.glyphWidth, alignment: .center)
            Text(text)
                .font(Theme.sans(Theme.textCaption))
                .foregroundStyle(Theme.textSubtle)
                .fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 0)
        }
    }

    private func fileRow(_ file: ChangedFile, loud: Bool) -> some View {
        HStack(spacing: ReviewMetrics.fileGutter) {
            Text(file.status)
                .font(Theme.mono(Theme.textCaption))
                .foregroundStyle(loud ? Theme.danger : Theme.textFaint)
                .frame(width: 9, alignment: .leading)
            Text(file.path)
                .font(Theme.mono(Theme.textCaption))
                .foregroundStyle(loud ? Theme.text : Theme.textMuted)
                .lineLimit(1)
                .truncationMode(.middle)
            Spacer(minLength: 0)
            Text(file.counts)
                .font(Theme.mono(Theme.textCaption))
                .foregroundStyle(Theme.textFaint)
        }
    }

    private func chipRow(_ chips: [ReviewChip]) -> some View {
        ReviewChipFlow(chips: chips)
    }
}

// MARK: - Chips

/// One derived fact: a 5pt dot and the sentence the board phrased.
///
/// A tinted ground for the two chips that carry a status, the flat chip wash
/// for the rest — so a row of five reads as one row with one thing in it,
/// rather than five things of equal weight.
struct ReviewChipView: View {
    let chip: ReviewChip

    var body: some View {
        HStack(spacing: ReviewMetrics.chipGap) {
            // round-ok: a status dot — the design's 5px, one per chip.
            Circle()
                .fill(dot)
                .frame(width: ReviewMetrics.chipDot, height: ReviewMetrics.chipDot)
            Text(chip.text)
                .font(Theme.sans(Theme.textCaption))
                .foregroundStyle(text)
                .lineLimit(1)
        }
        .padding(.horizontal, ReviewMetrics.chipPadding)
        .frame(height: ReviewMetrics.chipHeight)
        .background(background, in: RoundedRectangle(cornerRadius: Theme.radiusChip))
    }

    /// The dot's hue. The grounds come from `comet_board::effects`, which is
    /// where the meaning is decided; this only says which of the theme's hues
    /// each one wears. None is `Theme.danger` — the review's one loud hue
    /// belongs to the unclaimed set.
    private var dot: Color {
        switch chip.ground {
        case .neutral, .settled: return Theme.settled
        case .working: return Theme.warning
        case .unknown: return Theme.textFaint
        }
    }

    private var text: Color {
        switch chip.ground {
        // A checked fact that is not news: a settled dot, so the row reads at a
        // glance as "the board looked", with quiet text.
        case .neutral: return Theme.textMuted
        case .settled: return Theme.settled
        case .working: return Theme.warning
        // The one a reader must not mistake for either: no status colour at
        // all, because there is no status.
        case .unknown: return Theme.textSubtle
        }
    }

    private var background: Color {
        switch chip.ground {
        case .settled: return Theme.settled.opacity(ReviewMetrics.chipTint)
        case .working: return Theme.warning.opacity(ReviewMetrics.chipTint)
        case .neutral, .unknown: return Theme.chip
        }
    }
}

/// A row of chips that wraps, because the effects row is five sentences and a
/// phone is 393 points wide.
struct ReviewChipFlow: View {
    let chips: [ReviewChip]

    var body: some View {
        FlowLayout(spacing: ReviewMetrics.chipGap) {
            ForEach(Array(chips.enumerated()), id: \.offset) { _, chip in
                ReviewChipView(chip: chip)
            }
        }
    }
}

/// Left-to-right wrapping, which SwiftUI has no stack for.
struct FlowLayout: Layout {
    var spacing: CGFloat = 6

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews,
                      cache: inout ()) -> CGSize {
        let width = proposal.width ?? .infinity
        let rows = rows(subviews, width: width)
        let height = rows.reduce(CGFloat(0)) { $0 + $1.height } +
            spacing * CGFloat(max(0, rows.count - 1))
        let widest = rows.map(\.width).max() ?? 0
        return CGSize(width: min(width, max(widest, 0)), height: height)
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews,
                       cache: inout ()) {
        var y = bounds.minY
        for row in rows(subviews, width: bounds.width) {
            var x = bounds.minX
            for index in row.indices {
                let size = subviews[index].sizeThatFits(.unspecified)
                subviews[index].place(at: CGPoint(x: x, y: y), proposal: ProposedViewSize(size))
                x += size.width + spacing
            }
            y += row.height + spacing
        }
    }

    private struct Row {
        var indices: [Int] = []
        var width: CGFloat = 0
        var height: CGFloat = 0
    }

    private func rows(_ subviews: Subviews, width: CGFloat) -> [Row] {
        var out: [Row] = []
        var row = Row()
        for index in subviews.indices {
            let size = subviews[index].sizeThatFits(.unspecified)
            let next = row.indices.isEmpty ? size.width : row.width + spacing + size.width
            if !row.indices.isEmpty && next > width {
                out.append(row)
                row = Row()
                row.indices = [index]
                row.width = size.width
                row.height = size.height
            } else {
                row.indices.append(index)
                row.width = next
                row.height = max(row.height, size.height)
            }
        }
        if !row.indices.isEmpty { out.append(row) }
        return out
    }
}
