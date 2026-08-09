// What the board knows about its own throughput — the Swift half of
// `comet_proto::view::stats`, rule for rule (gh#143, gh#151).
//
// The numbers are gathered by `comet_board::stats` on whichever device owns
// `board.db`; this file is the *shape* plus the derivations a renderer needs,
// which is exactly the split the Rust module exists to make: a viewport reads
// a `BoardStats` reply without linking the board crate.
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
struct Landing: Decodable, Hashable {
    var merged: Int = 0
    var open: Int = 0
    var closedUnmerged: Int = 0
    var noPr: Int = 0

    var total: Int { merged + open + closedUnmerged + noPr }
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

/// Everything the board can say about its own throughput over a window.
///
/// Strictly decoded — no per-field defaults. A field whose name skewed would
/// otherwise arrive as a zero and read as a real one; the screen says
/// "unreadable" instead, which is the truth.
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

    var landing: Landing
    var friction: Friction

    /// Attempts started per day, oldest first, holes filled with zeroes.
    var daily: [DayBucket]
    var dailyTokens: [TokenDay]
    /// Dispatches by hour of the box's local day, 24 slots.
    var hourOfDay: [Int]

    var byWorkspace: [String: Int]
    var byRuntime: [String: Int]
    var bySource: [String: Int]
    var byAccount: [String: Int]
    var agentDispatched: Int

    var tokensByModel: [String: TokenUsage]
    var tokensByRuntime: [String: TokenUsage]

    /// Whether any attempt in the window reported tokens — the gate the token
    /// half of the screen renders behind. A wall of zeroes would say the work
    /// was free rather than that it was never metered.
    var hasTokens: Bool { attemptsWithTokens > 0 }

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

/// The label-and-value rows under "At a glance": where the work landed, what
/// it cost in time, and what it cost in friction.
///
/// One list rather than four cards, for the phone's version of the reason the
/// desktop collapsed them (gh#143): a fact per card is what makes a screen a
/// scroll.
func statsGlanceLines(_ stats: BoardStats) -> [(label: String, value: String)] {
    var rows: [(label: String, value: String)] = []
    let landing = stats.landing
    if landing.total > 0 {
        rows.append(("Merged", "\(landing.merged) of \(landing.total)"))
        if landing.open > 0 { rows.append(("In review", "\(landing.open)")) }
        if landing.closedUnmerged > 0 {
            rows.append(("Closed unmerged", "\(landing.closedUnmerged)"))
        }
        if landing.noPr > 0 { rows.append(("No pull request", "\(landing.noPr)")) }
    }
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
