// What the board knows about its own throughput — the Swift half of
// `comet_proto::view::stats`, rule for rule (gh#143, gh#151).
//
// The numbers are gathered by `comet_board::stats` on each device that owns a
// `board.db`; this file is the *shape* plus the derivations a renderer needs,
// which is exactly the split the Rust module exists to make: a viewport reads
// one `BoardStats` or their explicit `AggregateBoardStats` union without
// linking the board crate.
//
// Ported rather than shared for the reason every file in here is: no Rust runs
// on this device. The Rust tests in `crates/proto/src/view/stats.rs` are the
// specification; these functions must keep answering the way they do.
//
// **That is checked, not hoped for (gh#157).** A second implementation of a
// rule is how a phone comes to disagree with a laptop about a number somebody
// is deciding on, so the cases live outside both languages: `mod spec` in that
// Rust file writes every rule's inputs and expected outputs to
// `Spec/stats-spec.json` and fails if the checked-in file stops matching the
// Rust; `SpecRunner` (launch arg `-spec`, or `scripts/ios-stats-spec.sh`) runs
// the functions below against the same file. Whichever side moves is the side
// that fails. Each function names the Rust one it mirrors — if you change one,
// the fixture will tell you about the other.
//
// **Honest empties are the rule of the whole file.** `completionRate` is
// `nil` rather than 0% before anything has ended, a window that metered
// nothing shows a blank instead of a free-looking zero, and every token total
// is read beside the share of attempts it can actually account for. A number
// that quietly under-reports is worse than no number.

import Foundation

// MARK: - The wire shape

/// The four buckets a provider actually meters, Anthropic-normalized so they
/// can be added (`comet_proto::TokenUsage`). Every field defaults to zero —
/// the Rust struct is `#[serde(default)]`, and an absent bucket is a bucket
/// that spent nothing.
struct TokenUsage: Decodable, Hashable {
    var inputTokens: UInt64 = 0
    var outputTokens: UInt64 = 0
    var cacheReadTokens: UInt64 = 0
    var cacheCreationTokens: UInt64 = 0

    init(inputTokens: UInt64 = 0, outputTokens: UInt64 = 0,
         cacheReadTokens: UInt64 = 0, cacheCreationTokens: UInt64 = 0) {
        self.inputTokens = inputTokens
        self.outputTokens = outputTokens
        self.cacheReadTokens = cacheReadTokens
        self.cacheCreationTokens = cacheCreationTokens
    }

    private enum CodingKeys: String, CodingKey {
        case inputTokens, outputTokens, cacheReadTokens, cacheCreationTokens
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        inputTokens = try c.decodeIfPresent(UInt64.self, forKey: .inputTokens) ?? 0
        outputTokens = try c.decodeIfPresent(UInt64.self, forKey: .outputTokens) ?? 0
        cacheReadTokens = try c.decodeIfPresent(UInt64.self, forKey: .cacheReadTokens) ?? 0
        cacheCreationTokens = try c.decodeIfPresent(UInt64.self, forKey: .cacheCreationTokens) ?? 0
    }

    /// Every token the provider handled. A plain sum, sound only because the
    /// buckets are disjoint by construction.
    var total: UInt64 { inputTotal + outputTokens }

    /// All input, cached and not.
    var inputTotal: UInt64 { inputTokens + cacheReadTokens + cacheCreationTokens }

    var isZero: Bool { total == 0 }

    static func + (a: TokenUsage, b: TokenUsage) -> TokenUsage {
        TokenUsage(inputTokens: a.inputTokens + b.inputTokens,
                   outputTokens: a.outputTokens + b.outputTokens,
                   cacheReadTokens: a.cacheReadTokens + b.cacheReadTokens,
                   cacheCreationTokens: a.cacheCreationTokens + b.cacheCreationTokens)
    }
}

/// One day's dispatches, for the throughput chart.
struct DayBucket: Decodable, Hashable {
    /// `YYYY-MM-DD`, in the BOX's local reckoning — the day an operator means
    /// when they say "yesterday", and deliberately not re-derived from a phone
    /// in another timezone.
    var date: String
    /// Attempts *started* in the day.
    var dispatches: Int
    /// Of those, how many have since ended `done`.
    var done: Int
}

/// One day's tokens, index-aligned with `daily`.
struct TokenDay: Decodable, Hashable {
    var date: String
    var usage: TokenUsage
}

/// One row of a tally — a workspace, a runtime, a source, a person.
struct Tally: Decodable, Hashable, Identifiable {
    var id: String { label }
    var label: String
    var count: Int
}

/// One row of a *token* tally: `Tally`'s shape with a breakdown instead of a
/// count, so a table can show where the tokens went and not only how many.
struct TokenTally: Decodable, Hashable, Identifiable {
    var id: String { label }
    var label: String
    var usage: TokenUsage
}

/// Where the work ended up — the question a completion rate only half-answers:
/// an attempt can end `done` and leave a pull request nobody merged.
///
/// Four places, and the two at the bottom are the reason this is a shape and
/// not a merge count (gh#228). *Closed unmerged* is a pull request somebody
/// rejected or abandoned; *no PR raised* is an agent that settled having
/// produced nothing. They are the only numbers on a stats screen that say the
/// board wasted its time, and folding either into "in review" hides the loss
/// behind the one word that reads as patience.
///
/// `inFlight` is deliberately outside the four: work still running has not
/// landed anywhere, and counting it under *no PR raised* reports an agent that
/// is still typing as an agent that came back empty. Absent from an older
/// board's reply, which decodes as nothing running.
struct Landing: Decodable, Hashable {
    var merged: Int = 0
    var open: Int = 0
    var closedUnmerged: Int = 0
    var noPr: Int = 0
    var inFlight: Int = 0

    /// The four that landed — `comet_proto::view::stats::Landing::total`.
    var total: Int { merged + open + closedUnmerged + noPr }

    /// Every task this accounts for, landed or still going.
    var touched: Int { total + inFlight }

    func count(_ kind: LandingKind) -> Int {
        switch kind {
        case .merged: merged
        case .open: open
        case .closedUnmerged: closedUnmerged
        case .noPr: noPr
        }
    }

    /// The bar, as bands — `Landing::segments`. **All four, always**,
    /// including the empty ones: a legend that drops `Closed unmerged 0` is
    /// one a reader cannot tell from a surface that never counted losses.
    var segments: [LandingSegment] {
        LandingKind.all.map { kind in
            let count = count(kind)
            return LandingSegment(
                kind: kind,
                label: kind.label,
                count: count,
                fraction: total == 0 ? 0 : Double(count) / Double(total))
        }
    }

    /// `11 tasks` — the headline over the bar. Tasks, never attempts.
    var headline: String { "\(total) task\(total == 1 ? "" : "s")" }

    /// What the bar leaves out. `nil` when nothing is still running, because
    /// "0 still running" is a line that says nothing.
    var inFlightNote: String? {
        inFlight > 0 ? "\(inFlight) still running — not landed anywhere yet" : nil
    }
}

/// One of the four places work lands, as an identity rather than a label —
/// `comet_proto::view::stats::LandingKind`. The screen paints these from the
/// status ramp: merged is settled, an open pull request is review,
/// closed-unmerged is blocked, nothing-raised is the working amber.
enum LandingKind: String, Decodable, Hashable {
    case merged
    case open
    case closedUnmerged
    case noPr

    /// Best outcome first, so a bar reads left to right from landed to lost.
    static let all: [LandingKind] = [.merged, .open, .closedUnmerged, .noPr]

    /// `PR open` rather than `In review`: review is a state a human is in, and
    /// the fact here is that the branch exists and has not been taken.
    var label: String {
        switch self {
        case .merged: "Merged"
        case .open: "PR open"
        case .closedUnmerged: "Closed unmerged"
        case .noPr: "No PR raised"
        }
    }
}

/// One band of the landing bar: what it is, how many tasks, and the share of
/// the bar it takes.
struct LandingSegment: Decodable, Hashable, Identifiable {
    var id: String { kind.rawValue }
    var kind: LandingKind
    var label: String
    var count: Int
    /// Share of `Landing.total`, `0...1`. Zero when the window landed nothing.
    var fraction: Double
}

/// The friction numbers — the board reporting on how often the work had to be
/// done twice, and on its own misjudgements.
struct Friction: Decodable, Hashable {
    var retriedTasks: Int = 0
    /// Times the board closed an attempt and then caught its agent still
    /// working. Not retries — nobody dispatched anything.
    var earlySettles: Int = 0
    /// Times an attempt *entered* blocked: a transition count, not a tick count.
    var blockedEntries: Int = 0
    var overruns: Int = 0

    /// Nothing to report is worth saying in one place rather than four.
    var isClean: Bool {
        retriedTasks == 0 && earlySettles == 0 && blockedEntries == 0 && overruns == 0
    }
}

/// How close a window's attempts came to filling their context windows
/// (gh#271) — how much of what an agent could hold the work actually needed.
///
/// Three small numbers rather than a distribution, on purpose: the question is
/// whether attempts on this board routinely run out of context, which is a
/// fact about how the work is *shaped* and shows up nowhere in the spend.
/// Mirrors `stats::ContextPressure`.
struct ContextPressure: Decodable, Hashable {
    /// Attempts whose harness reported a window at all — the coverage for this
    /// half of the screen. The two below are shares of THIS, never of the
    /// window: one of the three harnesses meters no context at all.
    var attemptsReported: Int = 0
    /// …of which this many were last seen at or past the point their harness
    /// compacts (or 90% of the window, for a harness that names no point).
    var nearCompaction: Int = 0
    /// The fullest any one attempt was last seen, `0...100`. `nil` when
    /// nothing reported — never `0`, which would read as agents running empty.
    var peakPercent: Int?
}

// MARK: - Spend (gh#182)
//
// The board counts tokens; these are what it costs. Two facts, kept apart on
// purpose: the LIST PRICE of what the board ran (its own number, summed over
// the models it could price) and the SUBSCRIPTIONS behind that work (per
// account, entered by a person). They are never added together — on a box
// carrying several teammates' slots, one "cost" field would be summing other
// people's bills and calling it the board's spend.
//
// Amounts arrive as plain dollars. The rate arithmetic itself is the box's
// (`comet_board::prices`); this side only reads and phrases the result.

/// What one model costs per million tokens, in each of the four buckets
/// `TokenUsage` counts. Four rates and not one: a cache read is a tenth of
/// fresh input and a cache write a premium on it, and an agent replaying a long
/// context every turn is overwhelmingly cache reads.
struct ModelRate: Decodable, Hashable {
    var input: Double
    var output: Double
    var cacheRead: Double
    var cacheWrite: Double
}

/// One model's tokens, priced — with the rate it was priced at, because a
/// figure with no provenance is one nobody can check.
struct ModelSpend: Decodable, Hashable, Identifiable {
    var id: String { label }
    /// The model as the run reported it.
    var label: String
    /// The table row that priced it — shorter than `label` when a dated model
    /// id priced off its family.
    var rateKey: String
    /// `builtin` (the dated table inside the box's binary) or `config` (an
    /// override in its `routing.toml`).
    var source: String
    var rate: ModelRate
    var usage: TokenUsage
    var cost: Double
}

/// Which part of a harness run spent a token (gh#426).
enum AgentKind: String, Decodable, Hashable {
    case main
    case subagent
}

/// One main/subagent/model slice, where the journal exposed assistant-step
/// attribution. The amount is named as a list-price API estimate on the wire
/// because subscription runs are not billed per token.
struct AgentSpend: Decodable, Hashable, Identifiable {
    var agent: AgentKind
    var name: String?
    var model: String
    var usage: TokenUsage
    var listPriceApiEstimate: Double?
    var unpricedTokens: UInt64

    var id: String { "\(agent.rawValue)|\(name ?? "")|\(model)" }

    var label: String {
        let who = agent == .main ? "Main" : (name ?? "Subagent")
        return "\(who) · \(model)"
    }

    var priceLabel: String {
        guard let estimate = listPriceApiEstimate else { return "rates not configured" }
        if unpricedTokens > 0 && estimate == 0 { return "unpriced" }
        return humanUsd(estimate)
    }
}

/// A plan a human wrote down: what an agent account costs its owner per month.
struct AccountPlan: Decodable, Hashable {
    var label: String?
    var monthly: Double
}

/// What one account's work would have cost at the meter, beside what its
/// subscription actually costs. Never one number: see the section note.
struct AccountSpend: Decodable, Hashable, Identifiable {
    var id: String { label }
    var label: String
    var attempts: Int
    var usage: TokenUsage
    var listPrice: Double
    var unpricedTokens: UInt64
    /// `nil` is unconfigured, and not zero — comet never sees anybody's bill.
    var plan: AccountPlan?
    /// The plan's share of this window. `nil` for an all-time window, which
    /// cannot be pro-rated, or an unconfigured plan.
    var planInWindow: Double?

    /// How far the subscription carried it: list price as a multiple of what
    /// the plan cost over the same window. `nil` when either half is missing or
    /// the plan is free — a ratio against zero is not a number.
    /// Mirrors `stats::AccountSpend::subsidy`.
    var subsidy: Double? {
        guard let plan = planInWindow, plan != 0 else { return nil }
        return listPrice / plan
    }
}

/// The rate set a figure was computed from — the date included, because a
/// price list is a snapshot and a page that hid its age would be implying
/// freshness it does not have.
struct RateTable: Decodable, Hashable {
    /// `YYYY-MM-DD`, when the shipped half was last checked against published
    /// pricing.
    var asOf: String
    var entries: [String: ModelRate]
    /// Which of those the box's `routing.toml` overrode.
    var overridden: [String]
}

/// What the window cost.
struct BoardSpend: Decodable, Hashable {
    var rates: RateTable
    var listPrice: Double
    var byModel: [ModelSpend]
    /// Models with tokens and no rate. Present, never folded into the total: a
    /// breakdown that dropped what it could not price would not add up to the
    /// token counts on the same screen.
    var unpriced: [TokenTally]
    var unpricedTokens: UInt64
    var accounts: [AccountSpend]

    /// Did every metered model have a rate?
    /// Mirrors `stats::BoardSpend::is_complete`.
    var isComplete: Bool { unpriced.isEmpty }

    /// Any money to show at all.
    /// Mirrors `stats::BoardSpend::has_price`.
    var hasPrice: Bool { !byModel.isEmpty }

    /// The headline, said once — with what it could not account for attached
    /// rather than left implied.
    /// Mirrors `stats::BoardSpend::headline`.
    var headline: String {
        let price = humanUsd(listPrice)
        if isComplete { return "\(price) list-price API estimate" }
        return "\(price) list-price API estimate, plus \(humanTokens(unpricedTokens)) "
            + "unpriced token(s) across \(unpriced.count) model(s)"
    }

    /// What the operator pays per month across every account with a plan.
    /// Their plans, not the board's spend.
    /// Mirrors `stats::BoardSpend::monthly_subscriptions`.
    var monthlySubscriptions: Double {
        accounts.compactMap { $0.plan?.monthly }.reduce(0, +)
    }
}

/// The semantic basis for every compatible `listPrice` / `cost` money field
/// in a stats reply. Optional on `BoardStats` only so a new phone can still
/// read an older box; every newly emitted reply carries this discriminator.
enum PricingBasis: String, Decodable, Hashable {
    case listPriceApiEstimate
}

/// Everything the board can say about its own throughput over a window.
///
/// Strictly decoded — no per-field defaults. A field whose name skewed would
/// otherwise arrive as a zero and read as a real one; the screen says
/// "unreadable" instead, which is the truth. `spend` is the one exception and
/// for the opposite reason: absent *is* a state, and it means "rates not
/// configured".
struct BoardStats: Decodable, Hashable {
    /// The window these numbers cover, in days. `nil` means everything.
    var sinceDays: Int64?
    var attempts: Int
    var tasksTouched: Int
    /// Ended attempts by outcome name (`done`, `cancelled`, `failed`).
    var outcomes: [String: Int]
    var live: Int
    /// Attempts that ended `done` as a share of ended attempts. `nil` when
    /// nothing has ended — not the same as zero, and a page that drew it as 0%
    /// would be lying about a board that has just started.
    var completionRate: Double?

    var medianMinutes: Int64?
    var p90Minutes: Int64?
    var longestMinutes: Int64?
    var totalMinutes: Int64

    /// Tokens across every attempt in the window that reported any. Read it
    /// beside `attemptsWithTokens`: a total over the rows that answered, not
    /// over the window.
    var tokens: TokenUsage
    var attemptsWithTokens: Int
    /// That share, `0...1`. `nil` when nothing ran, never `0` — the same rule
    /// `completionRate` follows.
    var tokenCoverage: Double?
    /// Optional for older boxes: no value means the journal exposed no
    /// main/subagent attribution, never that those agents spent zero.
    var attemptsWithAgentUsage: Int?
    var agentUsage: [AgentSpend]?

    var landing: Landing
    var friction: Friction

    /// Attempts started per day, oldest first, holes filled with zeroes.
    var daily: [DayBucket]
    var dailyTokens: [TokenDay]
    /// Dispatches by hour of the box's local day, 24 slots.
    var hourOfDay: [Int]
    /// The same hours, split by the workspace they went to (gh#179) — the
    /// crossing the desktop's stats page draws instead of an hour card and a
    /// space card that each hide what the other knows.
    ///
    /// Optional because it is optional on the wire: a board older than the
    /// field answers without it, and this screen does not draw the grid yet
    /// either. Decoded rather than ignored so the phone's copy of the shape
    /// stays the whole shape — `comet_proto::view::stats::hour_grid` is the
    /// arithmetic to mirror on the day it does.
    var hoursByWorkspace: [String: [Int]]?

    var byWorkspace: [String: Int]
    var byRuntime: [String: Int]
    var bySource: [String: Int]
    var byAccount: [String: Int]
    var agentDispatched: Int

    var tokensByModel: [String: TokenUsage]
    var tokensByRuntime: [String: TokenUsage]
    /// Tokens by whose subscription paid for them. `byAccount` says who ran how
    /// many attempts; this says what those attempts spent.
    var tokensByAccount: [String: TokenUsage]

    /// What it cost, and what the plans behind it cost (gh#182). `nil` is
    /// **rates not configured** — a state said out loud rather than drawn as a
    /// confident `$0.00`. A board that was given rates and simply spent nothing
    /// arrives with a `spend` whose total is zero, and those are different
    /// facts.
    var spend: BoardSpend?
    var pricingBasis: PricingBasis?

    /// How close this window's attempts ran to filling their agents' context
    /// windows (gh#271) — the other meter, and the one the spend cannot stand
    /// in for: a compacting agent and a comfortable one cost about the same.
    ///
    /// Optional for `hoursByWorkspace`'s reason: a board older than the field
    /// answers without it, and an absent block must degrade to "nothing
    /// reported" rather than make the whole screen unreadable.
    var context: ContextPressure?

    /// Any attempt on record at all — all time, never windowed (gh#434): the
    /// dispatch evidence a host sweep settles on (`board_dispatched`'s answer,
    /// riding the stats reply). This screen leans on `BoardStore`'s own settled
    /// host instead of asking; decoded rather than ignored so the phone's copy
    /// of the shape stays the whole shape. Optional for `hoursByWorkspace`'s
    /// reason: a board older than the field answers without it.
    var dispatched: Bool?

    /// Whether any attempt in the window reported tokens — the gate the token
    /// half of the screen renders behind. A wall of zeroes would say the work
    /// was free rather than that it was never metered.
    var hasTokens: Bool { attemptsWithTokens > 0 }

    /// Did anything in this window report a context window at all? The gate
    /// the context line renders behind. `0 of 0` would read as a board with no
    /// context pressure rather than one with no measurements.
    /// Mirrors `stats::ContextPressure::is_reported`.
    var contextReported: Bool { (context?.attemptsReported ?? 0) > 0 }

    /// Is there a priced figure to show? False covers both halves of "no": no
    /// rates at all, and rates that matched none of the models this window ran.
    /// Mirrors `stats::BoardStats::has_spend`.
    var hasSpend: Bool { spend?.hasPrice ?? false }

    /// The sentence the money half leads with — including the two ways there is
    /// no number, which are the ones worth saying out loud.
    /// Mirrors `stats::BoardStats::spend_label`.
    var spendLabel: String {
        guard let spend else { return "rates not configured" }
        if spend.hasPrice { return spend.headline }
        if spend.unpricedTokens > 0 {
            return "no rate for any model in this window "
                + "(\(humanTokens(spend.unpricedTokens)) unpriced token(s))"
        }
        return "nothing metered to price"
    }

    /// Nothing ran. Distinct from "nothing finished".
    var isEmpty: Bool { attempts == 0 }

    /// How the window is named on screen.
    var windowLabel: String {
        switch sinceDays {
        case .some(1): return "last 24 hours"
        case .some(let days): return "last \(days) days"
        case nil: return "all time"
        }
    }
}

// MARK: - All boards (gh#461)

/// The shared `AggregateBoardStats` wire contract. The phone deliberately
/// decodes the same merged `BoardStats` the desktop and CLI render; it does not
/// reimplement host fan-out or arithmetic in Swift.
struct StatsDevice: Decodable, Hashable, Identifiable {
    var deviceId: String
    var label: String
    var id: String { deviceId }
}

enum StatsHostStatus: String, Decodable, Hashable {
    case answered, duplicate, noBoard, unreachable, unreadable, upgradeRequired, unknown

    init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = StatsHostStatus(rawValue: raw) ?? .unknown
    }

    var compromisesAggregate: Bool {
        self == .unreachable || self == .unreadable || self == .upgradeRequired || self == .unknown
    }
}

struct StatsHost: Decodable, Hashable, Identifiable {
    var device: StatsDevice
    var status: StatsHostStatus
    var boardId: String?
    var error: String?
    var upgrade: StatsUpgradeDetails?
    var id: String { device.deviceId }
}

struct StatsUpgradeDetails: Decodable, Hashable {
    var currentVersion: String
    var requiredVersion: String
    var error: String
    var canApply: Bool
}

struct AggregateBoardStatsSource: Decodable, Hashable, Identifiable {
    var boardId: String
    var host: StatsDevice
    var stats: BoardStats
    var id: String { boardId }
}

struct AggregateBoardStats: Decodable, Hashable {
    var sinceDays: Int64?
    var stats: BoardStats
    var boards: [AggregateBoardStatsSource]
    var hosts: [StatsHost]
    var complete: Bool

    /// Mirrors `AggregateBoardStats::completeness_note`, including its promise
    /// that missing hosts are never represented as zero activity.
    var completenessNote: String? {
        let missing = hosts.compactMap { host -> String? in
            guard host.status.compromisesAggregate else { return nil }
            if host.status == .upgradeRequired {
                return "\(host.device.label) is on v\(host.upgrade?.currentVersion ?? "unknown"); "
                    + "v\(host.upgrade?.requiredVersion ?? "unknown") is required for all-board stats"
            }
            return host.status == .unreadable
                ? "\(host.device.label) was unreadable"
                : "\(host.device.label) did not answer"
        }
        guard !missing.isEmpty else { return nil }
        return "Partial aggregate — \(missing.joined(separator: "; ")). "
            + "The totals include only the boards that answered."
    }
}

// MARK: - Derivations

/// One tally, ordered for reading: biggest first, ties alphabetical so the rows
/// do not shuffle between refreshes of an unchanged board.
/// Mirrors `stats::ranked` — pinned by the fixture's `rankedTop` cases.
func ranked(_ tally: [String: Int]) -> [Tally] {
    tally.map { Tally(label: $0.key, count: $0.value) }
        .sorted { $0.count == $1.count ? $0.label < $1.label : $0.count > $1.count }
}

/// The same, capped — with everything past the cap folded into one honest
/// `n others` row rather than dropped. A truncated list that does not say it
/// was truncated reads as the whole truth.
/// Mirrors `stats::ranked_top`.
func rankedTop(_ tally: [String: Int], _ max: Int) -> [Tally] {
    let rows = ranked(tally)
    guard max > 0, rows.count > max else { return rows }
    let tail = rows[max...]
    return Array(rows[..<max]) + [Tally(label: "\(tail.count) others",
                                        count: tail.reduce(0) { $0 + $1.count })]
}

/// `rankedTop` for a token tally: biggest total first, ties alphabetical, the
/// tail folded into one row that carries the usage it stands for. Rows that
/// spent nothing are dropped — a model that appears with four zeroes is noise
/// in a table about where tokens went.
/// Mirrors `stats::ranked_tokens`.
func rankedTokens(_ tally: [String: TokenUsage], _ max: Int) -> [TokenTally] {
    var rows: [TokenTally] = []
    for (label, usage) in tally where !usage.isZero {
        rows.append(TokenTally(label: label, usage: usage))
    }
    rows.sort { a, b in
        a.usage.total == b.usage.total ? a.label < b.label : a.usage.total > b.usage.total
    }
    guard max > 0, rows.count > max else { return rows }
    let tail = rows[max...]
    return Array(rows[..<max]) + [
        TokenTally(label: "\(tail.count) others",
                   usage: tail.reduce(TokenUsage()) { $0 + $1.usage })
    ]
}

/// A token count as a screen shows it: `812`, `48k`, `1.31M`.
///
/// Three significant figures and no more. Nobody acts on the difference
/// between 1,310,442 and 1,310,443, and a seven-digit figure is a number the
/// eye has to count digits in.
/// Mirrors `stats::human_tokens`.
func humanTokens(_ tokens: UInt64) -> String {
    let billion: UInt64 = 1_000_000_000
    let million: UInt64 = 1_000_000
    let thousand: UInt64 = 1_000
    let value = Double(tokens)
    if tokens >= billion { return String(format: "%.2fB", value / Double(billion)) }
    if tokens >= million { return String(format: "%.2fM", value / Double(million)) }
    if tokens >= 10 * thousand { return String(format: "%.0fk", value / Double(thousand)) }
    if tokens >= thousand { return String(format: "%.1fk", value / Double(thousand)) }
    return "\(tokens)"
}

/// An amount as a screen shows it: `$0.0042`, `$1.32`, `$248`.
///
/// Three tiers. Cents is the default scale money is read at; over a hundred
/// dollars they are noise on a figure nobody acts on to the cent, and under one
/// cent they are the difference between a real amount and a `$0.00` that reads
/// as free — which is the thing the whole spend half is designed against.
/// Mirrors `rates::human_usd`.
func humanUsd(_ dollars: Double) -> String {
    let abs = Swift.abs(dollars)
    if abs >= 100 { return String(format: "$%.0f", dollars) }
    if abs >= 0.01 || abs == 0 { return String(format: "$%.2f", dollars) }
    return String(format: "$%.4f", dollars)
}

/// A bar's share of its chart, `0...1`, scaled against the largest bucket —
/// not against the total: these charts answer "which day was busy", and a
/// proportion-of-total bar on a thirty-day window is thirty bars too short to
/// compare.
/// Mirrors `stats::bar_fraction` (which takes usizes; the `UInt64` overload
/// below is the same rule for the token series).
func barFraction(_ value: Int, _ peak: Int) -> Double {
    guard peak > 0 else { return 0 }
    return min(max(Double(value) / Double(peak), 0), 1)
}

func barFraction(_ value: UInt64, _ peak: UInt64) -> Double {
    guard peak > 0 else { return 0 }
    return min(max(Double(value) / Double(peak), 0), 1)
}

/// The busiest bucket in a day series — the scale every bar is drawn against.
/// Mirrors `stats::peak_dispatches`.
func peakDispatches(_ daily: [DayBucket]) -> Int {
    daily.map(\.dispatches).max() ?? 0
}

/// Mirrors `stats::peak_tokens`.
func peakTokens(_ daily: [TokenDay]) -> UInt64 {
    daily.map(\.usage.total).max() ?? 0
}

/// A duration in minutes, said the way a person would: `48m`, `3h 20m`, `2d 4h`.
/// Mirrors `stats::human_minutes`.
func humanMinutes(_ minutes: Int64) -> String {
    let m = Swift.max(0, minutes)
    if m < 60 { return "\(m)m" }
    let hours = m / 60
    let rem = m % 60
    if hours < 24 { return rem == 0 ? "\(hours)h" : "\(hours)h \(rem)m" }
    let days = hours / 24
    let remH = hours % 24
    return remH == 0 ? "\(days)d" : "\(days)d \(remH)h"
}

/// A rate as a whole-number percentage. `nil` stays `nil` all the way to the
/// renderer — see `BoardStats.completionRate`.
/// Mirrors `stats::percent` (renamed only because `percent` reads as a noun
/// at a Swift call site).
func percentLabel(_ rate: Double?) -> String? {
    guard let rate else { return nil }
    return String(format: "%.0f%%", min(max(rate, 0), 1) * 100)
}

/// The windows the stats screen offers, and what each is called — the same set
/// the desktop page and the CLI's `--since-days` use, so the surfaces are
/// asking one question.
/// Mirrors `stats::WINDOWS`, shortened for a phone's width.
let statsWindows: [(days: Int64?, label: String)] = [
    (1, "24h"), (7, "7d"), (30, "30d"), (nil, "All"),
]

// MARK: - The renderer's own
//
// Everything below this line has no counterpart in `comet_proto` and so is not
// in the fixture: it is how THIS screen phrases things, not a rule two
// surfaces have to agree on. `coverageNote` is the one borderline case — the
// desktop spells the same sentence in `StatsPage::coverage_note`, private to
// its page. If a third surface ever needs it, it belongs in proto with the
// rest, and in the fixture with them.

/// The coverage sentence — what share of the window's attempts the token
/// totals actually account for. The desktop page's `coverage_note`, word for
/// word.
///
/// It rides with every token figure rather than sitting in a panel of its own,
/// because a total read without it is a total read wrong: attempts that
/// predate the recording, and harnesses that meter nothing, are simply absent
/// from the sums.
func coverageNote(_ stats: BoardStats) -> String? {
    guard let share = percentLabel(stats.tokenCoverage) else { return nil }
    return "\(share) of attempts reported usage (\(stats.attemptsWithTokens) of \(stats.attempts))"
}

/// The headline's qualifying line: the facts that exist, in one sentence.
///
/// Assembled only from facts that exist — a board with nothing ended has no
/// rate and no median, and an em-dash for each would read as two failures
/// rather than as a board that has only just started.
func statsHeadlineFacts(_ stats: BoardStats) -> [String] {
    var facts: [String] = []
    if let rate = percentLabel(stats.completionRate) { facts.append("\(rate) ended in done") }
    if stats.landing.merged > 0 { facts.append("\(stats.landing.merged) merged") }
    if let median = stats.medianMinutes { facts.append("\(humanMinutes(median)) median") }
    if stats.live > 0 { facts.append("\(stats.live) running now") }
    return facts
}

/// `17 dispatches` — the count, in words that agree with themselves.
func statsHeadline(_ stats: BoardStats) -> String {
    "\(stats.attempts) dispatch\(stats.attempts == 1 ? "" : "es")"
}

/// The label-and-value rows under "At a glance": what the work cost in time,
/// and what it cost in friction.
///
/// One list rather than four cards, for the phone's version of the reason the
/// desktop collapsed them (gh#143): a fact per card is what makes a screen a
/// scroll. Where the work *landed* left this list in gh#228 — as four rows it
/// could only ever be a merge count with the losses folded away, so it is a
/// bar with a legend of its own.
func statsGlanceLines(_ stats: BoardStats) -> [(label: String, value: String)] {
    var rows: [(label: String, value: String)] = []
    rows.append(("Agent time", humanMinutes(stats.totalMinutes)))
    if let p90 = stats.p90Minutes { rows.append(("Nine in ten within", humanMinutes(p90))) }
    // Friction earns a line when there is any, and one honest line when there
    // is none — four zeroes would be four things to read that all say nothing
    // happened.
    let friction = stats.friction
    if friction.isClean {
        rows.append(("Friction", "none"))
    } else {
        if friction.retriedTasks > 0 { rows.append(("Retried", "\(friction.retriedTasks)")) }
        if friction.blockedEntries > 0 {
            rows.append(("Stopped to ask", "\(friction.blockedEntries)"))
        }
        if friction.earlySettles > 0 {
            rows.append(("Closed while working", "\(friction.earlySettles)"))
        }
        if friction.overruns > 0 { rows.append(("Past their cap", "\(friction.overruns)")) }
    }
    rows.append(("Released", stats.agentDispatched == stats.attempts
        ? "all by agents"
        : stats.agentDispatched == 0 ? "all by you" : "\(stats.agentDispatched) by agents"))
    return rows
}

/// The four token buckets as rows, in the order the desktop tiles read.
/// Only ever called behind `hasTokens` — a blank, never a zero.
func statsTokenLines(_ usage: TokenUsage) -> [(label: String, value: String)] {
    [
        ("Uncached input", humanTokens(usage.inputTokens)),
        ("Cached input", humanTokens(usage.cacheReadTokens)),
        ("Cache writes", humanTokens(usage.cacheCreationTokens)),
        ("Output", humanTokens(usage.outputTokens)),
    ]
}

/// `Aug 3` — a day bucket's date said short, for the ends of the chart's axis.
/// The string is the box's own day (`YYYY-MM-DD`) and is re-read, never
/// re-derived: a phone in another timezone must not rename the box's Tuesday.
func shortDay(_ date: String) -> String {
    let parts = date.split(separator: "-")
    guard parts.count == 3, let month = Int(parts[1]), let day = Int(parts[2]),
          (1...12).contains(month) else { return date }
    let names = ["Jan", "Feb", "Mar", "Apr", "May", "Jun",
                 "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"]
    return "\(names[month - 1]) \(day)"
}
