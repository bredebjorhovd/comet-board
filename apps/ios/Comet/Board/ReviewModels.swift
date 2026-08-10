// What one attempt is worth, on the phone (gh#256) — the wire shape of
// `ReadAttemptReview` and the reading the desktop's review card is built on.
//
// This is a second implementation of `comet_board::claims` and
// `comet_board::effects`, for the same reason `StatsModels.swift` is a second
// implementation of `comet_proto::view::stats`: no Rust runs on this device.
// Two implementations of one rule is how a phone comes to disagree with a
// laptop about whether an attempt is trustworthy — which, on the one screen
// whose whole job is to be trusted about what a run did, is the worst place in
// the app for it to happen.
//
// So the sentences and the glyphs live here, not in the view:
//
// - [`AttemptReview.verdict`] is the one line the screen leads with, and
//   [`AttemptReview.findings`] is the ordered list it comes off. The order is
//   the argument — the remainder first, because it is the only entry derived
//   from something the agent never touched.
// - [`AttemptReview.effectChips`] is the row above the claims (§gh#236). A
//   review the board never read gets one chip saying exactly that, rather than
//   five chips of reassurance nobody earned.
// - [`AttemptReview.claimMark`] is `✓` / `·` / `!`, and `✓` is reserved for a
//   claim something the agent did not author stands behind.
//
// Every one of those is checked against the Rust by `Spec/review-spec.json`,
// which `crates/board/tests/ios_review_spec.rs` writes and `ReviewSpecRunner`
// reads — the same cross-language fixture idiom as the stats screen (gh#157),
// and for the same reason.

import Foundation

// MARK: - The wire shape

/// What the agent was asked to do — the left-hand side of a review.
struct ReviewBrief: Decodable, Hashable {
    var identifier: String
    var title: String
    var url: String
    var body: String?
}

/// One file the attempt's branch touched, as git reports it.
struct ChangedFile: Decodable, Hashable {
    var path: String
    /// git's status letter: `A` added, `M` modified, `D` deleted.
    var status: String
    var added: Int
    var removed: Int
    var binary: Bool
    /// Identifiers this file's diff added or removed a line naming (§gh#235).
    var symbols: [String]

    enum CodingKeys: String, CodingKey {
        case path, status, added, removed, binary, symbols
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        path = try c.decode(String.self, forKey: .path)
        status = try c.decodeIfPresent(String.self, forKey: .status) ?? ""
        added = try c.decodeIfPresent(Int.self, forKey: .added) ?? 0
        removed = try c.decodeIfPresent(Int.self, forKey: .removed) ?? 0
        binary = try c.decodeIfPresent(Bool.self, forKey: .binary) ?? false
        symbols = try c.decodeIfPresent([String].self, forKey: .symbols) ?? []
    }

    init(path: String, status: String = "M", added: Int = 0, removed: Int = 0,
         binary: Bool = false, symbols: [String] = []) {
        self.path = path
        self.status = status
        self.added = added
        self.removed = removed
        self.binary = binary
        self.symbols = symbols
    }

    /// How much moved, as a reader wants it: `+18 −2`, or `binary` for a file
    /// that has no lines to count. One spelling, because the CLI's column, the
    /// desktop's row and this one are the same fact.
    var counts: String {
        binary ? "binary" : "+\(added) −\(removed)"
    }
}

/// How far a symbol anchor reaches, counted in both trees (§gh#236).
struct CallSites: Decodable, Hashable {
    var symbol: String
    var now: Int
    /// The count in the base tree, or nil when the board could not read it —
    /// which is not zero, and must never render as "new".
    var before: Int?
}

/// One claim, checked against the diff.
struct ClaimView: Decodable, Hashable {
    var text: String
    var files: [String]
    var symbols: [String]
    /// Changed files the claim's anchors — of either kind — account for.
    var matched: [String]
    /// Anchors the claim named that the diff does not contain.
    var unmatched: [String]
    var callSites: [CallSites]

    enum CodingKeys: String, CodingKey {
        case text, files, symbols, matched, unmatched
        case callSites = "call_sites"
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        text = try c.decode(String.self, forKey: .text)
        files = try c.decodeIfPresent([String].self, forKey: .files) ?? []
        symbols = try c.decodeIfPresent([String].self, forKey: .symbols) ?? []
        matched = try c.decodeIfPresent([String].self, forKey: .matched) ?? []
        unmatched = try c.decodeIfPresent([String].self, forKey: .unmatched) ?? []
        callSites = try c.decodeIfPresent([CallSites].self, forKey: .callSites) ?? []
    }

    init(text: String, files: [String] = [], symbols: [String] = [],
         matched: [String] = [], unmatched: [String] = [], callSites: [CallSites] = []) {
        self.text = text
        self.files = files
        self.symbols = symbols
        self.matched = matched
        self.unmatched = unmatched
        self.callSites = callSites
    }

    /// Did anything this claim named actually change? A claim where nothing did
    /// is the review's second-loudest row, after the unclaimed set.
    var anchored: Bool { !matched.isEmpty }

    /// Every anchor this claim carries, in the order it was written.
    var anchors: [String] { files + symbols }
}

/// Where the changed set came from — and, when it could not be had, why.
enum DiffSource: Decodable, Hashable {
    case checkout
    case recorded
    case unavailable(reason: String)

    private enum CodingKeys: String, CodingKey {
        case source, reason
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        switch try c.decodeIfPresent(String.self, forKey: .source) ?? "unavailable" {
        case "checkout": self = .checkout
        case "recorded": self = .recorded
        default:
            self = .unavailable(reason: try c.decodeIfPresent(String.self, forKey: .reason)
                ?? "the board did not say")
        }
    }

    var unavailableReason: String? {
        if case .unavailable(let reason) = self { return reason }
        return nil
    }
}

/// One verification command, and what it did across the whole run.
struct RunCheck: Decodable, Hashable {
    var command: String
    var runs: Int
    var failed: Int

    /// Did this command pass at least once? Not "did it pass last" — the
    /// journal's order is the order calls were made.
    var everPassed: Bool { failed < runs }
}

/// What a run's journal says it did, summed (§gh#183).
struct RunEvidence: Decodable, Hashable {
    var commands: Int
    var failed: Int
    var checks: [RunCheck]
    var truncated: Bool

    enum CodingKeys: String, CodingKey {
        case commands, failed, checks, truncated
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        commands = try c.decodeIfPresent(Int.self, forKey: .commands) ?? 0
        failed = try c.decodeIfPresent(Int.self, forKey: .failed) ?? 0
        checks = try c.decodeIfPresent([RunCheck].self, forKey: .checks) ?? []
        truncated = try c.decodeIfPresent(Bool.self, forKey: .truncated) ?? false
    }

    init(commands: Int = 0, failed: Int = 0, checks: [RunCheck] = [], truncated: Bool = false) {
        self.commands = commands
        self.failed = failed
        self.checks = checks
        self.truncated = truncated
    }

    /// Did anything at all check this work?
    var checked: Bool { !checks.isEmpty }

    /// Checks that never passed. What a review should read before any claim.
    var failing: [RunCheck] { checks.filter { !$0.everPassed } }
}

/// What the board is willing to say about a file, decided by its name.
///
/// Classified on the host — the phone reads the answer rather than the path,
/// so a rule the board changes cannot be re-derived differently here.
enum FileKind: String, Decodable, Hashable {
    case rust
    case typeScript = "type_script"
    case python
    case go
    case swift
    case otherCode = "other_code"
    case manifest
    case config
    case schema
    case prose
    case opaque

    /// Does the board know how this language spells a public declaration?
    var readsAPI: Bool {
        switch self {
        case .rust, .typeScript, .python, .go, .swift: return true
        default: return false
        }
    }

    /// Could this file carry a public API at all? A lockfile and a changelog
    /// cannot, so neither makes the answer unknown by being unreadable.
    var mayHoldAPI: Bool {
        switch self {
        case .otherCode, .opaque: return true
        default: return readsAPI
        }
    }
}

/// What one changed file's own diff says about it.
struct ReviewFileScan: Decodable, Hashable {
    var path: String
    var kind: FileKind?
    var testsAdded: Int
    var testsRemoved: Int
    var api: Int
    var schema: Int
    var configKeys: Int

    enum CodingKeys: String, CodingKey {
        case path, kind, api, schema
        case testsAdded = "tests_added"
        case testsRemoved = "tests_removed"
        case configKeys = "config_keys"
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        path = try c.decode(String.self, forKey: .path)
        kind = try c.decodeIfPresent(FileKind.self, forKey: .kind)
        testsAdded = try c.decodeIfPresent(Int.self, forKey: .testsAdded) ?? 0
        testsRemoved = try c.decodeIfPresent(Int.self, forKey: .testsRemoved) ?? 0
        api = try c.decodeIfPresent(Int.self, forKey: .api) ?? 0
        schema = try c.decodeIfPresent(Int.self, forKey: .schema) ?? 0
        configKeys = try c.decodeIfPresent(Int.self, forKey: .configKeys) ?? 0
    }

    init(path: String, kind: FileKind? = nil, testsAdded: Int = 0, testsRemoved: Int = 0,
         api: Int = 0, schema: Int = 0, configKeys: Int = 0) {
        self.path = path
        self.kind = kind
        self.testsAdded = testsAdded
        self.testsRemoved = testsRemoved
        self.api = api
        self.schema = schema
        self.configKeys = configKeys
    }

    var fileKind: FileKind { kind ?? .opaque }
}

/// Everything the board derived from the branch itself (§gh#236).
struct ReviewEffects: Decodable, Hashable {
    /// Did the board actually read this branch? False on every attempt whose
    /// checkout was gone before anything looked — and a false that rendered
    /// "Public API unchanged" would be the board asserting what it never
    /// checked.
    var read: Bool
    var files: [ReviewFileScan]
    var testsAfter: Int?
    var testsBefore: Int?
    var depsAdded: [String]
    var depsKnown: Bool

    enum CodingKeys: String, CodingKey {
        case read, files
        case testsAfter = "tests_after"
        case testsBefore = "tests_before"
        case depsAdded = "deps_added"
        case depsKnown = "deps_known"
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        read = try c.decodeIfPresent(Bool.self, forKey: .read) ?? false
        files = try c.decodeIfPresent([ReviewFileScan].self, forKey: .files) ?? []
        testsAfter = try c.decodeIfPresent(Int.self, forKey: .testsAfter)
        testsBefore = try c.decodeIfPresent(Int.self, forKey: .testsBefore)
        depsAdded = try c.decodeIfPresent([String].self, forKey: .depsAdded) ?? []
        depsKnown = try c.decodeIfPresent(Bool.self, forKey: .depsKnown) ?? false
    }

    init(read: Bool = false, files: [ReviewFileScan] = [], testsAfter: Int? = nil,
         testsBefore: Int? = nil, depsAdded: [String] = [], depsKnown: Bool = false) {
        self.read = read
        self.files = files
        self.testsAfter = testsAfter
        self.testsBefore = testsBefore
        self.depsAdded = depsAdded
        self.depsKnown = depsKnown
    }

    func file(_ path: String) -> ReviewFileScan? {
        files.first { $0.path == path }
    }
}

/// Everything a review of one attempt is made of (§gh#183) — the shape
/// `ReadAttemptReview` answers with, `snake_case` throughout and with the
/// remainder flattened into the top level.
struct AttemptReview: Decodable, Hashable {
    var taskId: String
    var attempt: Int64
    /// Which attempt of the task this is, 1-based.
    var attemptNumber: Int
    var state: String
    var outcome: String?
    var branch: String?
    var worktree: String?
    var prUrl: String?
    var brief: ReviewBrief
    /// When the agent submitted its claims. Nil with an empty claims list means
    /// it never answered the contract at all, which is a different fact from
    /// claiming nothing and must not render as one.
    var claimedAt: String?
    /// Why the block this attempt wrote could not be read (§gh#235).
    var claimsError: String?
    // ---- the remainder, flattened in ----
    var claims: [ClaimView]
    var unclaimed: [ChangedFile]
    var unmatchedAnchors: [String]
    var claimedCount: Int
    // ---- what the board could see for itself ----
    var changed: [ChangedFile]
    var diff: DiffSource
    var uncommitted: Int?
    var evidence: RunEvidence
    var effects: ReviewEffects

    enum CodingKeys: String, CodingKey {
        case attempt, state, outcome, branch, worktree, brief, claims, unclaimed
        case changed, diff, uncommitted, evidence, effects
        case taskId = "task_id"
        case attemptNumber = "attempt_number"
        case prUrl = "pr_url"
        case claimedAt = "claimed_at"
        case claimsError = "claims_error"
        case unmatchedAnchors = "unmatched_anchors"
        case claimedCount = "claimed"
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        taskId = try c.decode(String.self, forKey: .taskId)
        attempt = try c.decodeIfPresent(Int64.self, forKey: .attempt) ?? 0
        attemptNumber = try c.decodeIfPresent(Int.self, forKey: .attemptNumber) ?? 1
        state = try c.decodeIfPresent(String.self, forKey: .state) ?? ""
        outcome = try c.decodeIfPresent(String.self, forKey: .outcome)
        branch = try c.decodeIfPresent(String.self, forKey: .branch)
        worktree = try c.decodeIfPresent(String.self, forKey: .worktree)
        prUrl = try c.decodeIfPresent(String.self, forKey: .prUrl)
        brief = try c.decode(ReviewBrief.self, forKey: .brief)
        claimedAt = try c.decodeIfPresent(String.self, forKey: .claimedAt)
        claimsError = try c.decodeIfPresent(String.self, forKey: .claimsError)
        claims = try c.decodeIfPresent([ClaimView].self, forKey: .claims) ?? []
        unclaimed = try c.decodeIfPresent([ChangedFile].self, forKey: .unclaimed) ?? []
        unmatchedAnchors = try c.decodeIfPresent([String].self, forKey: .unmatchedAnchors) ?? []
        claimedCount = try c.decodeIfPresent(Int.self, forKey: .claimedCount) ?? 0
        changed = try c.decodeIfPresent([ChangedFile].self, forKey: .changed) ?? []
        diff = try c.decodeIfPresent(DiffSource.self, forKey: .diff) ?? .unavailable(
            reason: "the board did not say")
        uncommitted = try c.decodeIfPresent(Int.self, forKey: .uncommitted)
        evidence = try c.decodeIfPresent(RunEvidence.self, forKey: .evidence) ?? RunEvidence()
        effects = try c.decodeIfPresent(ReviewEffects.self, forKey: .effects) ?? ReviewEffects()
    }

    /// Did the agent answer the claim contract at all?
    var claimed: Bool { claimedAt != nil }

    /// Is every changed file accounted for?
    var complete: Bool { unclaimed.isEmpty }
}

// MARK: - The reading

/// Three states and not two, because "nothing is wrong" and "nothing was
/// established" are the opposite of the same thing.
enum ReviewTone: String, Equatable {
    /// The board can see something nobody accounted for. The one tone this
    /// screen is allowed to shout in.
    case alarm
    /// Nothing was established: no claims were submitted, or the diff could not
    /// be read at all.
    case unknown
    /// The claims account for the whole diff and nothing contradicts them.
    case settled

    /// Should a surface make this loud? Only `alarm`.
    var loud: Bool { self == .alarm }
}

/// Which fact a finding is. Surfaces switch on this rather than on the
/// sentence, so the wording can change without a renderer changing meaning.
enum ReviewFindingKind: String, Equatable {
    case unclaimed
    case uncommitted
    case unsupportedClaims = "unsupported_claims"
    case neverPassed = "never_passed"
    case unchecked
    case neverClaimed = "never_claimed"
    case malformedClaims = "malformed_claims"
    case noDiff = "no_diff"

    /// Does this one mean somebody has to look, or only that nothing is known?
    var tone: ReviewTone {
        switch self {
        case .unclaimed, .uncommitted, .unsupportedClaims, .neverPassed,
             .malformedClaims, .unchecked:
            return .alarm
        case .neverClaimed, .noDiff:
            return .unknown
        }
    }
}

/// One thing a review has to say out loud, already counted and phrased.
struct ReviewFinding: Equatable {
    var kind: ReviewFindingKind
    var text: String
}

/// What the whole review amounts to, in one line.
struct ReviewVerdict: Equatable {
    var tone: ReviewTone
    var text: String
}

/// How much a claim is standing on (§gh#236), in one glyph.
enum ClaimMark: String, Equatable {
    /// Nothing this claim anchored moved. `!`, in the review's one loud hue.
    case contradicted
    /// Something the agent did not author supports it. `✓`.
    case corroborated
    /// The anchors matched, and nothing corroborates it. `·`, quiet.
    case bare

    /// The glyph itself, so the CLI, the desktop and the phone cannot pick
    /// different ones.
    var glyph: String {
        switch self {
        case .contradicted: return "!"
        case .corroborated: return "✓"
        case .bare: return "·"
        }
    }
}

/// What a chip is standing on, and so how loudly it reads.
enum ReviewGround: String, Equatable {
    /// A fact that is not news: the board checked, and the surface held.
    case neutral
    /// Corroboration — evidence *for* something a claim asserted.
    case settled
    /// Not wrong, but a thing to look at.
    case working
    /// The board could not answer. Never rendered as either of the first two.
    case unknown
}

/// One derived fact, phrased once, for every surface that draws it.
struct ReviewChip: Equatable {
    var text: String
    var ground: ReviewGround
}

/// Whether the tests were run, and how it went — off the run journal, never off
/// a claim.
enum TestRun: Equatable {
    case passing
    case failing(Int)
    case neverRun

    var phrase: String {
        switch self {
        case .passing: return "all passing"
        case .failing: return "not passing"
        case .neverRun: return "never run"
        }
    }

    var ground: ReviewGround {
        switch self {
        case .passing: return .neutral
        case .failing, .neverRun: return .working
        }
    }
}

/// One of the four "did this move" questions, answered.
enum ReviewSurface: Equatable {
    case unchanged
    case changed(Int)
    case unknown(reason: String)

    var ground: ReviewGround {
        switch self {
        case .unchanged: return .neutral
        case .changed: return .working
        case .unknown: return .unknown
        }
    }
}

/// `1 file` / `2 files` — said the same way everywhere rather than `file(s)`.
func reviewCount(_ n: Int, _ singular: String, _ plural: String) -> String {
    n == 1 ? "\(n) \(singular)" : "\(n) \(plural)"
}

/// The first line of a refusal, for a verdict bar that gets one line.
func reviewFirstLine(_ message: String) -> String {
    (message.split(separator: "\n", omittingEmptySubsequences: false).first
        .map(String.init) ?? message)
        .trimmingCharacters(in: .whitespaces)
}

// MARK: The run journal

/// Commands that mean "I ran the tests" — the subset of `comet_board`'s
/// `VERIFICATION` list that runs a suite, spelled exactly as it is spelled
/// there so a command cannot be a test runner here and not one on the box.
let reviewTestCommands: [String] = [
    "cargo test", "cargo nextest", "cargo bench",
    "npm test", "npm run test", "pnpm test", "pnpm run test", "yarn test",
    "bun test", "jest", "vitest", "mocha", "playwright test",
    "pytest", "python -m pytest", "python -m unittest", "tox", "nox",
    "go test", "make test", "swift test", "gradle test", "./gradlew test",
    "mvn test", "dotnet test", "rspec", "phpunit",
]

private func reviewWordChar(_ c: Character) -> Bool {
    c.isASCII && (c.isLetter || c.isNumber || c == "_")
}

/// Drop leading `KEY=value` assignments and `env`/`time`/`nice` wrappers, so
/// `CI=1 cargo test` and `time cargo test` are both `cargo test`.
private func reviewStripEnv(_ segment: String) -> String {
    var rest = String(segment.drop { $0 == "(" || $0 == " " || $0 == "\t" })
    while true {
        guard let token = rest.split(separator: " ", omittingEmptySubsequences: true).first
            .map(String.init) else { return rest }
        let wrapper = ["env", "time", "nice", "sudo", "exec"].contains(token)
        let assignment: Bool = {
            guard let eq = token.firstIndex(of: "=") else { return false }
            let key = token[token.startIndex..<eq]
            return !key.isEmpty && key.allSatisfy(reviewWordChar)
        }()
        if !wrapper && !assignment { return rest }
        // The token is the first non-space run, so skipping it and the space
        // after it is what "the rest of the line" means here.
        guard let tokenRange = rest.range(of: token) else { return rest }
        let after = rest[tokenRange.upperBound...]
        guard let first = after.first, first.isWhitespace else { return rest }
        rest = String(after.drop { $0.isWhitespace })
    }
}

/// The command lines inside one command: split on `&&`, `||`, `;`, `|` and
/// newlines, each trimmed of the shell noise that can precede a verb.
private func reviewSegments(_ command: String) -> [String] {
    var parts = [command]
    for separator in ["\n", ";", "&&", "||", "|"] {
        parts = parts.flatMap { $0.components(separatedBy: separator) }
    }
    return parts.map(reviewStripEnv)
}

/// Does this command run a *test suite*, as opposed to checking something else
/// (§gh#236)? A prefix match with a word boundary on the far end, so `cargo
/// testbed` is not `cargo test`.
func reviewRunsTests(_ command: String) -> Bool {
    reviewSegments(command).contains { segment in
        reviewTestCommands.contains { verb in
            guard segment.hasPrefix(verb) else { return false }
            let rest = segment.dropFirst(verb.count)
            return rest.isEmpty || !reviewWordChar(rest[rest.startIndex])
        }
    }
}

/// Read the run journal for what happened to the tests.
func reviewTestRun(_ evidence: RunEvidence) -> TestRun {
    let tests = evidence.checks.filter { reviewRunsTests($0.command) }
    if tests.isEmpty { return .neverRun }
    let failing = tests.filter { !$0.everPassed }.count
    return failing > 0 ? .failing(failing) : .passing
}

// MARK: The effects row

extension ReviewEffects {
    /// Did the public surface move? Unknown the moment one changed file is code
    /// the board has no rule for — a surface answer that ignored the file it
    /// could not read would be worse than no answer.
    var publicAPI: ReviewSurface {
        let unread = files.filter { $0.fileKind.mayHoldAPI && !$0.fileKind.readsAPI }
        let count = files.reduce(0) { $0 + $1.api }
        if count > 0 { return .changed(count) }
        guard let first = unread.first else { return .unchanged }
        let reason = unread.count == 1
            ? "\(first.path) is not a language the board reads"
            : "\(unread.count) changed files are in languages the board does not read, "
                + "starting with \(first.path)"
        return .unknown(reason: reason)
    }

    /// Did the schema move? Never unknown: the rule is a list of statements and
    /// a list of paths, checked over every changed file whatever language it is
    /// in.
    var schemaSurface: ReviewSurface {
        let count = files.reduce(0) { $0 + $1.schema }
        return count > 0 ? .changed(count) : .unchanged
    }

    /// Config keys the branch added.
    var configSurface: ReviewSurface {
        let count = files.reduce(0) { $0 + $1.configKeys }
        return count > 0 ? .changed(count) : .unchanged
    }

    /// `Tests 41 → 47, all passing` — the two counts from the two trees, the
    /// verdict from the journal. Both, because either alone is misleading.
    func testsChip(_ run: TestRun) -> ReviewChip {
        let added = files.reduce(0) { $0 + $1.testsAdded }
        var ground = run.ground
        // A suite that got smaller is a thing to look at whatever the run said:
        // every test still passing is exactly what deleting the failing one
        // looks like from here.
        if let before = testsBefore, let after = testsAfter, after < before {
            ground = .working
        }
        if let before = testsBefore, let after = testsAfter {
            return ReviewChip(text: "Tests \(before) → \(after), \(run.phrase)", ground: ground)
        }
        if added > 0 {
            return ReviewChip(text: "\(reviewCount(added, "test", "tests")) added, \(run.phrase)",
                              ground: ground)
        }
        return ReviewChip(text: "Tests \(run.phrase)", ground: ground)
    }

    /// The effects row: one chip per question, in the order a reviewer asks
    /// them. A review the board never read gets one chip saying exactly that,
    /// rather than five chips of reassurance nobody earned.
    func chips(_ evidence: RunEvidence) -> [ReviewChip] {
        guard read else {
            return [ReviewChip(text: "no effects read from this branch", ground: .unknown)]
        }
        var out = [testsChip(reviewTestRun(evidence))]
        let api = publicAPI
        switch api {
        case .unchanged:
            out.append(ReviewChip(text: "Public API unchanged", ground: api.ground))
        case .changed(let count):
            out.append(ReviewChip(
                text: "\(reviewCount(count, "public item", "public items")) changed",
                ground: api.ground))
        case .unknown:
            out.append(ReviewChip(text: "Public API unknown", ground: api.ground))
        }
        let schema = schemaSurface
        if case .changed(let count) = schema {
            out.append(ReviewChip(
                text: "\(reviewCount(count, "schema line", "schema lines")) changed",
                ground: schema.ground))
        } else {
            out.append(ReviewChip(text: "Schema unchanged", ground: schema.ground))
        }
        let config = configSurface
        if case .changed(let count) = config {
            out.append(ReviewChip(
                text: "\(reviewCount(count, "config key", "config keys")) added",
                ground: config.ground))
        } else {
            out.append(ReviewChip(text: "No config keys added", ground: config.ground))
        }
        if !depsKnown {
            out.append(ReviewChip(text: "Dependencies unknown", ground: .unknown))
        } else if depsAdded.isEmpty {
            out.append(ReviewChip(text: "No dependencies added", ground: .neutral))
        } else {
            out.append(ReviewChip(
                text: "\(reviewCount(depsAdded.count, "dependency", "dependencies")) added",
                ground: .working))
        }
        return out
    }
}

/// The evidence that attaches to one claim: what the board found in the files
/// that claim's own anchors reached.
func reviewClaimChips(matched: [String], callSites: [CallSites],
                      effects: ReviewEffects, run: TestRun) -> [ReviewChip] {
    var out: [ReviewChip] = []
    let added = matched.compactMap { effects.file($0) }.reduce(0) { $0 + $1.testsAdded }
    if added > 0 {
        let tests = reviewCount(added, "new test", "new tests")
        switch run {
        case .passing:
            out.append(ReviewChip(text: "\(tests) \(added == 1 ? "passes" : "pass")",
                                  ground: .settled))
        case .failing:
            out.append(ReviewChip(text: "\(tests), not passing", ground: .working))
        case .neverRun:
            out.append(ReviewChip(text: "\(tests), never run", ground: .working))
        }
    }
    for sites in callSites {
        let now = reviewCount(sites.now, "call site", "call sites")
        let text: String
        switch sites.before {
        case .some(let before) where before == sites.now: text = "\(now), unchanged"
        case .some(0): text = "\(now), new"
        case .some(let before): text = "\(now), was \(before)"
        case .none: text = now
        }
        out.append(ReviewChip(text: text, ground: .settled))
    }
    // The sentence the design asks for by name, and the reason the glyph drops
    // from `✓` to `·`: no test in this branch reaches anything this claim is
    // anchored to. Said whenever it is true, even beside a call-site chip —
    // "somebody calls it" and "something checks it" are not the same news.
    if added == 0 && effects.read && !matched.isEmpty {
        out.append(ReviewChip(text: "no test covers this", ground: .working))
    }
    return out
}

/// Is there anything here that *supports* a claim, as opposed to merely
/// describing it? What decides whether a claim's glyph is `✓` or `·`.
func reviewCorroborated(_ chips: [ReviewChip]) -> Bool {
    chips.contains { $0.ground == .settled }
}

// MARK: The verdict

extension AttemptReview {
    /// Everything the board can see that the agent did not account for,
    /// loudest first.
    ///
    /// Ordered, not sorted: the sequence is the argument. The remainder leads
    /// because it is the only entry derived from something the agent never
    /// touched; uncommitted work follows because it is the one fact that can
    /// make an *empty* remainder wrong; then the claims the diff refuses to
    /// support, then what did and did not get checked.
    func findings() -> [ReviewFinding] {
        var out: [ReviewFinding] = []
        // No diff means no denominator: every other finding below would be
        // computed against an empty changed set and would read as good news.
        if let reason = diff.unavailableReason {
            return [ReviewFinding(
                kind: .noDiff,
                text: "there is no diff to check these claims against: \(reason)")]
        }
        let files = reviewCount(changed.count, "changed file", "changed files")
        if let error = claimsError {
            out.append(ReviewFinding(
                kind: .malformedClaims,
                text: "this attempt wrote a claims block the board could not read, "
                    + "so nothing accounts for its \(files): \(reviewFirstLine(error))"))
        } else if !claimed {
            out.append(ReviewFinding(
                kind: .neverClaimed,
                text: "this attempt never answered the claim contract, "
                    + "so nothing accounts for its \(files)"))
        } else if !unclaimed.isEmpty {
            // Said as a proportion on purpose: "4 unclaimed" is a number, "4 of
            // 17" is the question of whether the summary covered the work.
            out.append(ReviewFinding(
                kind: .unclaimed,
                text: "\(unclaimed.count) of \(changed.count) changed files "
                    + "are claimed by nobody"))
        }
        if let n = uncommitted, n > 0 {
            out.append(ReviewFinding(
                kind: .uncommitted,
                text: "\(reviewCount(n, "file is", "files are")) changed in the checkout "
                    + "and on no branch, so the diff above has never seen them"))
        }
        let unsupported = claims.filter { !$0.anchored }.count
        if unsupported > 0 {
            out.append(ReviewFinding(
                kind: .unsupportedClaims,
                text: "\(reviewCount(unsupported, "claim is", "claims are")) "
                    + "anchored to files nothing happened to"))
        }
        let never = evidence.failing.count
        if never > 0 {
            out.append(ReviewFinding(
                kind: .neverPassed,
                text: "\(reviewCount(never, "check", "checks")) ran and never once passed"))
        } else if !evidence.checked && evidence.commands > 0 {
            out.append(ReviewFinding(
                kind: .unchecked,
                text: "nothing that verifies anything ran, across "
                    + reviewCount(evidence.commands, "command", "commands")))
        }
        return out
    }

    /// The one line a surface leads with.
    ///
    /// The loudest finding, or — when there is none — the sentence that says
    /// the claims covered the diff. Never silent: a review with nothing to
    /// report still has to say what it checked, or an empty screen reads as a
    /// screen that failed to load.
    func verdict() -> ReviewVerdict {
        if let finding = findings().first {
            return ReviewVerdict(tone: finding.kind.tone, text: finding.text)
        }
        if changed.isEmpty {
            return ReviewVerdict(tone: .unknown, text: "this attempt's branch changed nothing")
        }
        if changed.count == 1 {
            return ReviewVerdict(tone: .settled, text: "the one changed file is accounted for")
        }
        return ReviewVerdict(tone: .settled,
                             text: "all \(changed.count) changed files are accounted for")
    }

    /// The effects row (§gh#236): what the board found in the branch itself,
    /// beside what the agent said about it.
    func effectChips() -> [ReviewChip] {
        effects.chips(evidence)
    }

    /// The evidence chips under one claim.
    func claimChips(_ claim: ClaimView) -> [ReviewChip] {
        reviewClaimChips(matched: claim.matched, callSites: claim.callSites,
                         effects: effects, run: reviewTestRun(evidence))
    }

    /// Which glyph one claim is drawn with, in every surface that draws claims.
    func claimMark(_ claim: ClaimView) -> ClaimMark {
        if !claim.anchored { return .contradicted }
        return reviewCorroborated(claimChips(claim)) ? .corroborated : .bare
    }

    /// What moved, as the strip that closes the body says it (§gh#238).
    var diffTotals: (label: String, added: Int, removed: Int) {
        let added = changed.reduce(0) { $0 + $1.added }
        let removed = changed.reduce(0) { $0 + $1.removed }
        let label = changed.count == 1 ? "1 file changed" : "\(changed.count) files changed"
        return (label, added, removed)
    }
}

// MARK: Whose turn it is

/// The header pill (§gh#238): a task the board parked in `review` is waiting
/// for a human, one `blocked` on a question is waiting for the same human about
/// something else, and an attempt still running or long since done gets no pill
/// — a badge on every review is not a signal.
///
/// `answered` is this session's receipt rather than anything durable, and it
/// wins over the board's state because it is newer: the state moves when the
/// sync loop next reads the pull request, which is some seconds after the
/// button was pressed and not before.
///
/// Never the blocked hue, including for `blocked`: that one belongs to the
/// unclaimed set, and a pill in the header wearing it would be the loudest
/// thing on the screen saying what the remainder block is there to say.
struct TurnPill: Equatable {
    var label: String
    var status: Status
}

func reviewTurnPill(state: String, answered: Bool) -> TurnPill? {
    if answered { return TurnPill(label: "Answered", status: .settled) }
    // The wire value, not `BoardState.parse` — an unrecognised state must get
    // no pill at all, where `parse`'s lenient fallback would give it `ready`'s
    // silence by accident rather than on purpose.
    switch BoardState(rawValue: state.lowercased()) {
    case .review: return TurnPill(label: "Waiting on you", status: .review)
    case .blocked: return TurnPill(label: "Blocked on you", status: .review)
    default: return nil
    }
}

// MARK: - The verdict, going out (§gh#239)

/// What a reviewer can send. `comment` is the one that needs words; the other
/// two carry the remainder on their own.
enum VerdictKind: String, CaseIterable, Identifiable {
    case comment
    case approve
    case changesRequested = "changes_requested"

    var id: String { rawValue }

    /// The button's words, spelled as the desktop's `VERDICTS` spells them.
    var label: String {
        switch self {
        case .comment: return "Comment"
        case .approve: return "Approve"
        case .changesRequested: return "Request changes"
        }
    }

    /// The same verdict as a thing that has happened, for a sentence about one
    /// already sent — `comet_board::verdict::VerdictKind::label`.
    var pastTense: String {
        switch self {
        case .comment: return "commented"
        case .approve: return "approved"
        case .changesRequested: return "changes requested"
        }
    }

    /// Does this verdict have to carry prose? GitHub refuses a `COMMENT` or a
    /// `REQUEST_CHANGES` with an empty body, and it is right to: a verdict with
    /// nothing in it tells the agent to change something unnamed.
    var needsComment: Bool { self != .approve }
}

/// What came back from `SubmitVerdict`: where it landed, and what did not.
struct VerdictReceipt: Decodable, Hashable {
    var taskId: String
    var attempt: Int64
    var kind: String
    var reviewId: Int64
    /// Whether it reached the pull request.
    var posted: Bool
    var chatId: String?
    /// Whether it reached the checkout the agent is still in.
    var delivered: Bool
    var notDelivered: String?
    var unclaimed: Int
    var payload: String

    enum CodingKeys: String, CodingKey {
        case attempt, kind, posted, delivered, unclaimed, payload
        case taskId = "task_id"
        case reviewId = "review_id"
        case chatId = "chat_id"
        case notDelivered = "not_delivered"
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        taskId = try c.decodeIfPresent(String.self, forKey: .taskId) ?? ""
        attempt = try c.decodeIfPresent(Int64.self, forKey: .attempt) ?? 0
        kind = try c.decodeIfPresent(String.self, forKey: .kind) ?? ""
        reviewId = try c.decodeIfPresent(Int64.self, forKey: .reviewId) ?? 0
        posted = try c.decodeIfPresent(Bool.self, forKey: .posted) ?? false
        chatId = try c.decodeIfPresent(String.self, forKey: .chatId)
        delivered = try c.decodeIfPresent(Bool.self, forKey: .delivered) ?? false
        notDelivered = try c.decodeIfPresent(String.self, forKey: .notDelivered)
        unclaimed = try c.decodeIfPresent(Int.self, forKey: .unclaimed) ?? 0
        payload = try c.decodeIfPresent(String.self, forKey: .payload) ?? ""
    }

    init(taskId: String, attempt: Int64 = 0, kind: String = "", reviewId: Int64 = 0,
         posted: Bool = false, chatId: String? = nil, delivered: Bool = false,
         notDelivered: String? = nil, unclaimed: Int = 0, payload: String = "") {
        self.taskId = taskId
        self.attempt = attempt
        self.kind = kind
        self.reviewId = reviewId
        self.posted = posted
        self.chatId = chatId
        self.delivered = delivered
        self.notDelivered = notDelivered
        self.unclaimed = unclaimed
        self.payload = payload
    }
}

/// Is this verdict worth interrupting the agent with? A bare approval is not.
func reviewWorthDelivering(_ kind: VerdictKind, _ comment: String) -> Bool {
    kind == .changesRequested || !comment.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
}

/// The sentence the screen states before you submit, and the one the receipt
/// confirms after — `comet_board::verdict::contract_line`.
///
/// `delivering` is whether the payload is going into a chat at all. Saying
/// "delivered" when it is not would be the screen promising something it is
/// about to not do.
func reviewContractLine(_ review: AttemptReview, delivering: Bool) -> String {
    guard delivering else {
        return "Posted to the pull request. Nothing is delivered into the chat."
    }
    let target = review.branch ?? "the authoring chat"
    let attached: String
    switch review.unclaimed.count {
    case 0: attached = ""
    case 1: attached = ", with the unclaimed change attached"
    // The design's own phrasing for the case it draws.
    case 2: attached = ", with both unclaimed changes attached"
    case let n: attached = ", with all \(n) unclaimed changes attached"
    }
    return "Delivered into \(target) once\(attached)."
}

/// One sentence for where a verdict landed — the desktop's `receipt_line`.
///
/// "Posted and delivered" and "posted, and the author is gone" are different
/// facts, and a receipt that said only the first would be the phone claiming
/// something it was told did not happen.
func reviewReceiptLine(_ receipt: VerdictReceipt) -> String {
    // The idempotent path: this exact verdict was already submitted.
    let posted = receipt.posted
        ? "Posted on the pull request"
        : "Already on the pull request"
    switch (receipt.delivered, receipt.notDelivered) {
    case (true, _):
        return "\(posted), and delivered into the chat once."
    case (false, .some(let why)):
        return "\(posted). Nothing was delivered into the chat: \(why)."
    case (false, .none):
        return "\(posted)."
    }
}
