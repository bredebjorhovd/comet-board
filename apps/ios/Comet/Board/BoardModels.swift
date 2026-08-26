// Board vocabulary on the phone — a port of `crates/proto/src/view/board.rs`.
//
// The architecture rule the Rust module states about itself applies with more
// force here: the section order, the glyphs, the elapsed/cap spellings and the
// billing words are a published contract, said by four surfaces already (the
// desktop panel, the TUI, the `comet-board` CLI, the upstream comment). A
// viewport that spells any of them differently is a bug, not a dialect — so
// these are ports, line for line, and the constants below cite their source.
//
// What is deliberately NOT ported: `row_metadata`'s fixed-width column block
// and the `f`/`/` filter cycle. The first is terminal layout (`fixed(…, 12)`
// pads to a monospace grid that does not exist here) and the second is a
// keyboard affordance; the *content* decisions inside `state_metadata` — which
// facts each state is worth saying — are ported as `BoardRowDetail`, and laid
// out by the row view.

import Foundation

// MARK: - State

/// Board-level task state. Note the divergence from herdr's vocabulary the
/// Rust enum documents: herdr's `done` is our `review`; our `done` means the
/// issue is closed.
enum BoardState: String, CaseIterable, Hashable {
    case blocked, working, ready, review, failed, done

    /// Fixed section order: blocked → working → ready → review → failed → done.
    static let sectionOrder: [BoardState] = [.blocked, .working, .ready, .review, .failed, .done]

    /// An unrecognized wire value reads as `ready` — a schema-skewed row must
    /// read as waiting, never crash the view.
    static func parse(_ raw: String) -> BoardState {
        BoardState(rawValue: raw.lowercased()) ?? .ready
    }

    /// Shape-distinct glyph per state. Three shape families on purpose —
    /// pointed (`▲ ▸`), round (`● ·`), crossed (`✓ ✕`) — so every state
    /// survives colour being stripped.
    var glyph: String {
        switch self {
        case .blocked: return "▲"
        case .working: return "●"
        case .ready: return "▸"
        case .review: return "✓"
        case .failed: return "✕"
        case .done: return "·"
        }
    }

    /// The PUBLISHED spelling — `BLOCKED`, `DONE TODAY`. The TUI, the CLI, the
    /// desktop and this app all say it, and a contract is not something one
    /// viewport edits.
    var label: String {
        switch self {
        case .blocked: return "BLOCKED"
        case .working: return "WORKING"
        case .ready: return "READY"
        case .review: return "REVIEW"
        case .failed: return "FAILED"
        // The header says "DONE today" and it means it — see `sections`.
        case .done: return "DONE TODAY"
        }
    }

    /// A section header's words on THIS surface — a port of the desktop
    /// panel's `section_title` (gh#176), which the phone never got.
    ///
    /// Caps are a typographic choice, not part of the vocabulary, and the
    /// canvas has stopped making it: a header shouting in a grey slab was loud
    /// without being clear. Same words, sentence case (ios.md C2.1).
    var sectionTitle: String {
        switch self {
        case .blocked: return "Blocked"
        case .working: return "Working"
        case .ready: return "Ready"
        case .review: return "Review"
        case .failed: return "Failed"
        case .done: return "Done today"
        }
    }

    /// Holds a chat (and a concurrency slot): the live-attempt states.
    var holdsPane: Bool { self == .working || self == .blocked }
}

// MARK: - The wire row

/// One task, in the shape `WatchBoard` streams: herdr-board's `list --json`
/// contract with the pane→chat rename applied (docs/agent-conventions.md).
///
/// Snake_case on the wire — the Rust `TaskRow` carries no `rename_all`, because
/// the contract is herdr-board's and ported tooling reads it.
struct TaskRow: Decodable, Hashable, Identifiable {
    var id: String
    var identifier: String
    var title: String
    var state: String
    var source: String
    var url: String
    var labels: [String]
    /// False when no route matches, or the issue is gone upstream: the task is
    /// on the board but cannot be dispatched, and `dispatch` refuses it.
    var dispatchable: Bool
    /// The issue behind this row no longer exists upstream.
    var gone: Bool
    var route: String?
    /// The comet space (herdr's workspace — the config key keeps that name).
    var workspace: String?
    var runtime: String?
    /// The live attempt's chat (herdr-board's `pane_id`).
    var chatId: String?
    /// The chat that authored the latest review, retained after it settles.
    var reviewChatId: String?
    var prUrl: String?
    var prNumber: Int?
    var branch: String?
    var dispatchedBy: String?
    /// The chat the dispatch ran from, when an agent was in it. Both this and
    /// `dispatchedBy` null means the operator released it.
    var dispatchedByChat: String?
    /// How the most recent *ended* attempt ended: `done`, `failed`, `cancelled`
    /// or `orphaned`. Stays set while a newer attempt is live.
    var lastOutcome: String?
    var lastOutcomeAt: String?
    var attempts: Int
    var reopened: Int
    /// When the issue last changed upstream (RFC 3339). Bounds the `done`
    /// section to today.
    var updatedAt: String
    /// When the live attempt started (RFC 3339), for the elapsed counter.
    var startedAt: String?
    /// The agent-account slot this row's attempt spends. Null is the device's
    /// own CLI login.
    var account: String?
    /// The human whose frontend released this attempt, as that frontend named
    /// them. A claim, not a credential.
    var dispatchedByUser: String?
    /// Whose subscription the attempt actually spends, as an email (gh#101).
    var billedTo: String?
    /// The wall-clock cap one attempt on this row gets (gh#70), in seconds.
    /// Null is uncapped. On the wire because the elapsed counter is worth
    /// nothing without it, and the routing config lives on the board's host.
    var maxDurationSecs: Int?
    /// Why this row's live attempt stopped, when its harness could say
    /// (gh#545). A usage limit is the one whose fix is a decision — switch
    /// model, switch account, wait — rather than plain retry.
    var stopReason: StopReason?

    enum CodingKeys: String, CodingKey {
        case id, identifier, title, state, source, url, labels, dispatchable, gone
        case route, workspace, runtime, branch, attempts, reopened, account
        case chatId = "chat_id"
        case reviewChatId = "review_chat_id"
        case prUrl = "pr_url"
        case prNumber = "pr_number"
        case dispatchedBy = "dispatched_by"
        case dispatchedByChat = "dispatched_by_chat"
        case lastOutcome = "last_outcome"
        case lastOutcomeAt = "last_outcome_at"
        case updatedAt = "updated_at"
        case startedAt = "started_at"
        case dispatchedByUser = "dispatched_by_user"
        case billedTo = "billed_to"
        case maxDurationSecs = "max_duration_secs"
        case stopReason = "stop_reason"
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        identifier = try c.decode(String.self, forKey: .identifier)
        title = try c.decode(String.self, forKey: .title)
        state = try c.decode(String.self, forKey: .state)
        source = try c.decodeIfPresent(String.self, forKey: .source) ?? ""
        url = try c.decodeIfPresent(String.self, forKey: .url) ?? ""
        labels = try c.decodeIfPresent([String].self, forKey: .labels) ?? []
        dispatchable = try c.decodeIfPresent(Bool.self, forKey: .dispatchable) ?? false
        gone = try c.decodeIfPresent(Bool.self, forKey: .gone) ?? false
        route = try c.decodeIfPresent(String.self, forKey: .route)
        workspace = try c.decodeIfPresent(String.self, forKey: .workspace)
        runtime = try c.decodeIfPresent(String.self, forKey: .runtime)
        chatId = try c.decodeIfPresent(String.self, forKey: .chatId)
        reviewChatId = try c.decodeIfPresent(String.self, forKey: .reviewChatId)
        prUrl = try c.decodeIfPresent(String.self, forKey: .prUrl)
        prNumber = try c.decodeIfPresent(Int.self, forKey: .prNumber)
        branch = try c.decodeIfPresent(String.self, forKey: .branch)
        dispatchedBy = try c.decodeIfPresent(String.self, forKey: .dispatchedBy)
        dispatchedByChat = try c.decodeIfPresent(String.self, forKey: .dispatchedByChat)
        lastOutcome = try c.decodeIfPresent(String.self, forKey: .lastOutcome)
        lastOutcomeAt = try c.decodeIfPresent(String.self, forKey: .lastOutcomeAt)
        attempts = try c.decodeIfPresent(Int.self, forKey: .attempts) ?? 0
        reopened = try c.decodeIfPresent(Int.self, forKey: .reopened) ?? 0
        updatedAt = try c.decodeIfPresent(String.self, forKey: .updatedAt) ?? ""
        startedAt = try c.decodeIfPresent(String.self, forKey: .startedAt)
        account = try c.decodeIfPresent(String.self, forKey: .account)
        dispatchedByUser = try c.decodeIfPresent(String.self, forKey: .dispatchedByUser)
        billedTo = try c.decodeIfPresent(String.self, forKey: .billedTo)
        maxDurationSecs = try c.decodeIfPresent(Int.self, forKey: .maxDurationSecs)
        stopReason = try c.decodeIfPresent(StopReason.self, forKey: .stopReason)
    }

    /// Memberwise init for demo rows and tests (the decoder owns the wire).
    init(id: String, identifier: String, title: String, state: BoardState,
         source: String = "github", url: String = "", labels: [String] = [],
         dispatchable: Bool = true, gone: Bool = false, route: String? = nil,
         workspace: String? = nil, runtime: String? = nil, chatId: String? = nil,
         reviewChatId: String? = nil,
         prUrl: String? = nil, prNumber: Int? = nil, branch: String? = nil,
         dispatchedBy: String? = nil, dispatchedByChat: String? = nil,
         lastOutcome: String? = nil, lastOutcomeAt: String? = nil,
         attempts: Int = 0, reopened: Int = 0, updatedAt: String = "",
         startedAt: String? = nil, account: String? = nil,
         dispatchedByUser: String? = nil, billedTo: String? = nil,
         maxDurationSecs: Int? = nil, stopReason: StopReason? = nil) {
        self.id = id
        self.identifier = identifier
        self.title = title
        self.state = state.rawValue
        self.source = source
        self.url = url
        self.labels = labels
        self.dispatchable = dispatchable
        self.gone = gone
        self.route = route
        self.workspace = workspace
        self.runtime = runtime
        self.chatId = chatId
        self.reviewChatId = reviewChatId
        self.prUrl = prUrl
        self.prNumber = prNumber
        self.branch = branch
        self.dispatchedBy = dispatchedBy
        self.dispatchedByChat = dispatchedByChat
        self.lastOutcome = lastOutcome
        self.lastOutcomeAt = lastOutcomeAt
        self.attempts = attempts
        self.reopened = reopened
        self.updatedAt = updatedAt
        self.startedAt = startedAt
        self.account = account
        self.dispatchedByUser = dispatchedByUser
        self.billedTo = billedTo
        self.maxDurationSecs = maxDurationSecs
        self.stopReason = stopReason
    }

    var boardState: BoardState { BoardState.parse(state) }

    /// The live attempt's start instant, when the row carries one.
    var startedAtDate: Date? { rfc3339(startedAt) }
}

/// Why a runtime cannot start on the board's host (gh#187).
///
/// Two axes named apart, because they are two different jobs for whoever reads
/// them: a missing CLI is an install, a signed-out one is a login. The raw
/// values are `comet_proto::view::board::RuntimeUnavailable`'s camelCase
/// spellings; an unknown one decodes as `.unknown` rather than failing the
/// whole catalog, so a newer box cannot blank the phone's picker.
enum RuntimeUnavailable: String, Decodable, Hashable {
    case notInstalled
    case signedOut
    case unsupported
    case unknown

    init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = RuntimeUnavailable(rawValue: raw) ?? .unknown
    }

    /// The short phrase a chip carries — the same words the desktop picker and
    /// `comet-board doctor` use, so an operator who learned them in one place
    /// recognises them in the next.
    var reason: String {
        switch self {
        case .notInstalled: return "not installed"
        case .signedOut: return "signed out"
        case .unsupported: return "no adapter in this build"
        case .unknown: return "unavailable"
        }
    }

    /// What to do about it, where there is anything to do. The same two verbs
    /// `RuntimeUnavailable::hint` gives every other surface.
    var hint: String? {
        switch self {
        case .notInstalled: return "install its CLI on the host"
        case .signedOut: return "sign it in on the host"
        case .unsupported, .unknown: return nil
        }
    }
}

/// One runtime a dispatch can be pointed at, as `ListBoardRuntimes` reports it.
/// `name` is exactly what the `DispatchTask` override accepts; `harness` is how
/// the account picker knows which saved logins a runtime could spend (gh#74).
struct BoardRuntime: Decodable, Hashable, Identifiable {
    var name: String
    var label: String
    var harness: String
    /// Why the host could not start this one (gh#187), or nil when it could.
    /// Absent from a box too old to say, which reads as available — exactly
    /// what such a box used to promise.
    var unavailable: RuntimeUnavailable?

    var id: String { name }

    var available: Bool { unavailable == nil }

    /// What the row says under its label: why it cannot run, and what to do —
    /// nil on a runtime that can.
    var note: String? {
        guard let unavailable else { return nil }
        guard let hint = unavailable.hint else { return unavailable.reason }
        return "\(unavailable.reason) — \(hint)"
    }

    init(name: String, label: String, harness: String,
         unavailable: RuntimeUnavailable? = nil) {
        self.name = name
        self.label = label
        self.harness = harness
        self.unavailable = unavailable
    }
}

/// A saved agent login on the board's host, as `ListAgentAccounts` reports it.
struct BoardAccount: Decodable, Hashable, Identifiable {
    var id: String
    var harness: String
    var email: String?
    var planLabel: String?
    var active: Bool
    var displayName: String?

    /// What a chip says: the login's email, else the name the harness reported,
    /// else the slot id — never nothing, since the chip is how the operator
    /// tells whose limits a dispatch will spend.
    var label: String {
        if let email, !email.isEmpty { return email }
        if let displayName, !displayName.isEmpty { return displayName }
        return id
    }
}

// MARK: - The leading token (gh#125)

/// The `owner/repo` a GitHub task id names — `gh:Florin-AS/tally#507` →
/// `Florin-AS/tally`. Nil for a Linear id, which names no repo. Port of the
/// Rust `gh_repo`; `!` is the pull-request form of the id.
func ghRepo(_ taskId: String) -> String? {
    guard taskId.hasPrefix("gh:") else { return nil }
    return taskId.dropFirst(3)
        .split(whereSeparator: { $0 == "#" || $0 == "!" })
        .first.map(String.init)
}

/// Just the repository's name — `Florin-AS/tally` → `tally`. The owner is
/// noise when you work with a handful of repos; the name is the part you read.
func ghRepoName(_ taskId: String) -> String? {
    ghRepo(taskId)?.split(separator: "/").last.map(String.init)
}

extension TaskRow {
    /// The leading token a board row shows: the CLI's repo-qualified form,
    /// humanized — `tally #507`, not `gh#507` (gh#125). GitHub numbers issues
    /// per repository, so the bare identifier is ambiguous across repos. A
    /// Linear identifier (`LIN-142`) is already unique and shows unchanged, as
    /// does any id this rule cannot parse.
    ///
    /// The separator is the id's own, `#` or `!` — this qualifies a name, it
    /// does not replace one (gh#357). `gh!508` is the identifier of a pull
    /// request nobody filed a ticket for, and `tally #508` would be a second
    /// name for it, already spoken for by issue #508.
    var displayIdentifier: String {
        let separators: Set<Character> = ["#", "!"]
        guard let ix = id.lastIndex(where: { separators.contains($0) }) else {
            return identifier
        }
        let number = String(id[id.index(after: ix)...])
        guard let repo = ghRepoName(id), !repo.isEmpty,
              !number.isEmpty, number.allSatisfy({ $0.isASCII && $0.isNumber })
        else {
            return identifier
        }
        return "\(repo) \(id[ix])\(number)"
    }

    /// Whether this row's name *is* its pull request — a `gh!508` row, ported
    /// from `TaskRow::is_pull_request` (gh#357). Naming it and saying where it
    /// lives are one act, so the row says one of them.
    var isPullRequest: Bool {
        guard let number = prNumber, let ix = id.lastIndex(of: "!") else { return false }
        return String(id[id.index(after: ix)...]) == String(number)
    }

    /// The short slug of this row's title (gh#364), for the places that name a
    /// task in a token and have no room for the title. Not for a board row,
    /// which draws the title itself.
    var slug: String? { titleSlug(title) }
}

// MARK: - The descriptive half of a name (gh#364)

/// How many content words a slug carries, and the width it is capped at —
/// `view::slug::SLUG_WORDS` / `SLUG_MAX`.
private let slugWords = 3
private let slugMax = 28

/// The words that say nothing about *which* task this is — a port of
/// `view::slug::STOPWORDS`, and it has to stay in step with it: a branch is
/// named on the box from the Rust list, and a phone that stripped a different
/// set would render a different slug for the same task.
///
/// Language-scoped, and a list rather than a law: English, plus the Norwegian
/// that is about one title in nine on this board. Norwegian entries are spelled
/// folded (`på` is `pa`), because `asciiFold` runs before the lookup.
///
/// Negations (`no`, `not`, `ikke`), particles (`up`, `out`, `off`, `down`) and
/// quantities (`one`, `once`, `only`, `two`) are content here, deliberately —
/// see the Rust list for what each is protecting, including why `var` is absent
/// from the Norwegian half.
private let slugStopwords: Set<String> = [
    "a", "an", "the",
    "and", "or", "but", "nor", "so", "if", "because", "while", "when", "whether", "than", "then",
    "that", "what", "which", "who", "whom", "whose",
    "of", "to", "in", "on", "at", "for", "with", "from", "by", "as", "into", "onto", "about",
    "after", "before", "between", "through", "during", "per", "via", "over", "under", "against",
    "within", "without", "upon",
    "this", "these", "those", "there", "here", "it", "its", "they", "them", "their", "we", "our",
    "us", "you", "your", "i", "my", "me", "he", "him", "his", "she", "her", "hers",
    "is", "are", "was", "were", "be", "been", "being", "am", "do", "does", "did", "done", "has",
    "have", "had", "having", "can", "cannot", "could", "should", "would", "will", "shall", "may",
    "might", "must", "let",
    "just", "even", "still", "really", "actually", "simply", "quite", "rather", "also", "very",
    "too",
    "og", "eller", "men", "hvis", "pa", "av", "med", "som", "til", "fra", "en", "et", "den", "det",
    "de", "er", "kan", "skal", "vil", "har", "ved", "om",
]

/// The ASCII a Latin letter is spelled with when ASCII is all there is — a port
/// of `view::slug::fold`, and the half of this file most worth keeping in step:
/// `å→a`, `ø→o`, `æ→ae` is what every Norwegian system already does, and what
/// the box will have put in the branch name.
///
/// Nil means the letter has no ASCII spelling at all (Cyrillic, CJK), which
/// drops its word rather than guessing at it.
private func asciiFold(_ c: Character) -> String? {
    switch Character(c.lowercased()) {
    case "å": return "a"
    case "ø": return "o"
    case "æ": return "ae"
    case "à", "á", "â", "ã", "ä", "ā", "ă", "ą": return "a"
    case "è", "é", "ê", "ë", "ē", "ė", "ę": return "e"
    case "ì", "í", "î", "ï", "ī", "į": return "i"
    case "ò", "ó", "ô", "õ", "ö", "ō": return "o"
    case "ù", "ú", "û", "ü", "ū", "ů": return "u"
    case "ý", "ÿ": return "y"
    case "ñ", "ń": return "n"
    case "ç", "ć", "č": return "c"
    case "š", "ś": return "s"
    case "ž", "ź", "ż": return "z"
    case "ł": return "l"
    case "đ", "ð": return "d"
    case "ř": return "r"
    case "ť": return "t"
    case "œ": return "oe"
    case "ß": return "ss"
    case "þ": return "th"
    default: return nil
    }
}

/// A slug of a title: up to `slugWords` content words, joined with `-`, capped
/// at `slugMax` characters. Nil when the title yields nothing — which every
/// caller must handle, because the identifier alone is always enough.
///
/// A port of `view::slug::title_slug`, down to the rules that look like details
/// and are not: an apostrophe does not split a word (`task's` is `tasks`, not
/// `task` + `s`); a non-ASCII letter is folded rather than dropped (`Kjør` is
/// `kjor`), and only a letter with no ASCII spelling at all drops its word; and
/// a one-character word is not a word, because `⌘K åpner søket` is about the
/// search and not about the letter `k`.
///
/// Swift's `Character` is a grapheme cluster, so a decomposed `å` arrives as one
/// `Character` and folds like the precomposed one — the combining-mark case the
/// Rust side handles per `char`.
func titleSlug(_ title: String) -> String? {
    var words: [String] = []
    for raw in title.split(whereSeparator: { !$0.isLetter && !$0.isNumber && $0 != "'" && $0 != "\u{2019}" }) {
        var word = ""
        var droppable = false
        for c in raw {
            if c.isASCII && (c.isLetter || c.isNumber) {
                word += c.lowercased()
            } else if c == "'" || c == "\u{2019}" {
                continue
            } else if let ascii = asciiFold(c) {
                word += ascii
            } else if c.isLetter || c.isNumber {
                droppable = true
                break
            }
        }
        // A lone letter and a function word are the same kind of nothing.
        if droppable || word.count < 2 || slugStopwords.contains(word) { continue }
        words.append(word)
        if words.count == slugWords { break }
    }
    var out = ""
    for word in words {
        if out.isEmpty {
            out = String(word.prefix(slugMax))
            continue
        }
        if out.count + 1 + word.count > slugMax { break }
        out += "-" + word
    }
    return out.isEmpty ? nil : out
}

// MARK: - Sections

/// The board's sections in order, empty ones omitted.
///
/// `done` is bounded to today: every issue ever closed in a tracked repo
/// derives to `done`, and ninety-odd of them is a wall of history, not a board.
func boardSections(_ rows: [TaskRow], now: Date = Date()) -> [(state: BoardState, rows: [TaskRow])] {
    BoardState.sectionOrder.compactMap { state in
        let section = rows.filter { $0.boardState == state }
            .filter { state != .done || finishedToday($0, now: now) }
        return section.isEmpty ? nil : (state, section)
    }
}

/// One route's rows inside a section — the unit a hundred-row board is scanned
/// by (gh#125). Nil route is the `no route` group.
struct BoardSectionGroup: Hashable {
    var route: String?
    var rows: [TaskRow]

    /// What the group header says: the route's name, or `no route` — the words
    /// the rows themselves use.
    var label: String { route ?? noRouteLabel }

    /// Unrouted rows are visibility-only by design, so their group starts
    /// folded: worth a headline and a count, never pole position over rows a
    /// tap can actually dispatch. (The phone has no `f`/`/` cycle, so the
    /// Rust rule's filter half does not apply here.)
    var startsCollapsed: Bool { route == nil }
}

/// `boardSections`, with each section's rows grouped by route — the port of
/// `grouped_sections`. Biggest group first (the list reads as a ranking), ties
/// alphabetical so equal groups do not trade places between frames, and
/// `no route` last regardless of size: it must never hold the top of a section.
func groupedBoardSections(_ rows: [TaskRow], now: Date = Date())
    -> [(state: BoardState, groups: [BoardSectionGroup])]
{
    boardSections(rows, now: now).map { state, rows in
        var groups: [BoardSectionGroup] = []
        for row in rows {
            if let ix = groups.firstIndex(where: { $0.route == row.route }) {
                groups[ix].rows.append(row)
            } else {
                groups.append(BoardSectionGroup(route: row.route, rows: [row]))
            }
        }
        groups.sort { a, b in
            if (a.route == nil) != (b.route == nil) { return b.route == nil }
            if a.rows.count != b.rows.count { return a.rows.count > b.rows.count }
            return (a.route ?? "") < (b.route ?? "")
        }
        return (state, groups)
    }
}

/// Whether a section draws group headers at all: one routed group is readable
/// bare, a lone `no route` group still needs the header that keeps it folded.
func boardGroupHeadersShown(_ groups: [BoardSectionGroup]) -> Bool {
    groups.count > 1 || (groups.count == 1 && groups[0].route == nil)
}

/// Evidence that a board has ever been dispatched from: any row with an
/// attempt on record (gh#125). This is what the automatic host sweep settles
/// on — a frame proves a board *exists*, not that it is the org's board, and a
/// stale test board must lose to the box everyone works from. `attempts`, not
/// `chatId`: the chat id rides only the live attempt, and the box between
/// dispatches must not read as furniture.
func boardDispatched(_ rows: [TaskRow]) -> Bool {
    rows.contains { $0.attempts > 0 }
}

/// Was this task closed today, in the operator's own timezone? Local midnight,
/// not a rolling 24 hours: "today" is a thing a person means.
func finishedToday(_ row: TaskRow, now: Date = Date()) -> Bool {
    // An unparseable timestamp is not evidence of recency.
    guard let updated = rfc3339(row.updatedAt) else { return false }
    return Calendar.current.isDate(updated, inSameDayAs: now)
}

/// RFC 3339 with or without fractional seconds — the board writes both.
func rfc3339(_ text: String?) -> Date? {
    guard let text, !text.isEmpty else { return nil }
    let withFraction = ISO8601DateFormatter()
    withFraction.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
    if let date = withFraction.date(from: text) { return date }
    let plain = ISO8601DateFormatter()
    plain.formatOptions = [.withInternetDateTime]
    return plain.date(from: text)
}

// MARK: - Durations

/// `12s` / `9m04s` / `1h20m`. Minute resolution was rejected upstream: a
/// counter that never visibly moves is not worth the redraw.
func formatElapsed(_ secs: Int) -> String {
    let s = max(0, secs)
    if s < 60 { return "\(s)s" }
    if s < 3600 { return String(format: "%dm%02ds", s / 60, s % 60) }
    return String(format: "%dh%02dm", s / 3600, (s % 3600) / 60)
}

/// A *cap* said the way a person configured it: `2h`, `45m`, `1h30m`.
/// Deliberately not `formatElapsed` — a cap is a round number somebody typed
/// into `routing.toml`, and `2h00m` makes the reader check the minutes.
func formatCap(_ secs: Int) -> String {
    if secs < 60 { return "\(secs)s" }
    let (h, m) = (secs / 3600, (secs % 3600) / 60)
    if h == 0 { return "\(m)m" }
    if m == 0 { return "\(h)h" }
    return String(format: "%dh%02dm", h, m)
}

/// Seconds since an attempt started. Never negative: clock skew must not read
/// as a count-up from the future.
func agentElapsedSecs(startedAt: Date?, now: Date) -> Int? {
    guard let startedAt else { return nil }
    return max(0, Int(now.timeIntervalSince(startedAt)))
}

/// Past the route's cap — gh#70's clock is warning it now and will cancel it
/// next. The *decision* stays board-side; this is the display's half.
func agentOverCap(startedAt: Date?, capSecs: Int?, now: Date) -> Bool {
    guard let elapsed = agentElapsedSecs(startedAt: startedAt, now: now),
          let cap = capSecs else { return false }
    return elapsed >= cap
}

/// `1h50m / 2h` — or bare elapsed on an uncapped route, or nothing at all
/// where the row cannot say when it started.
func agentElapsedLabel(startedAt: Date?, capSecs: Int?, now: Date) -> String? {
    guard let secs = agentElapsedSecs(startedAt: startedAt, now: now) else { return nil }
    let elapsed = formatElapsed(secs)
    guard let cap = capSecs else { return elapsed }
    return "\(elapsed) / \(formatCap(cap))"
}

/// Coarser form for a fact that is not counting up — `12s`, `4m`, `3h`. Ported
/// from `format_age`.
func formatAge(_ secs: Int) -> String {
    let s = max(0, secs)
    if s < 60 { return "\(s)s" }
    if s < 3600 { return "\(s / 60)m" }
    return "\(s / 3600)h"
}

// MARK: - What rows say

/// What the route column renders for a row nothing routes.
let noRouteLabel = "no route"

/// The per-state facts a row is worth saying, ported from `state_metadata` —
/// the content decisions, with the terminal's column padding left behind.
struct BoardRowDetail {
    /// The lead line under the title: where it would go, how it is going, or
    /// what is waiting on you.
    var text: String
    /// The elapsed counter, kept separate so the row can re-read the clock on
    /// its own frames rather than rebuilding the string.
    var elapsed: String?
    var overCap: Bool
    /// The cross-billing note (gh#101), when this run charges somebody else.
    var billing: String?
}

func boardRowDetail(_ row: TaskRow, now: Date = Date()) -> BoardRowDetail {
    var text = ""
    var elapsed: String?
    var overCap = false
    switch row.boardState {
    case .working, .blocked:
        text = [row.runtime, row.workspace.map { "ws:\($0)" }]
            .compactMap { $0 }.filter { !$0.isEmpty }.joined(separator: " · ")
        elapsed = agentElapsedLabel(startedAt: row.startedAtDate,
                                    capSecs: row.maxDurationSecs, now: now)
        overCap = agentOverCap(startedAt: row.startedAtDate,
                               capSecs: row.maxDurationSecs, now: now)
    case .failed:
        text = "pane exited without completing"
    case .review:
        // What the row needs, then where it lives (gh#357). The row is already
        // named — its identifier is the line above this one — and a bare
        // `PR #12` at the head of the sub-line is a second number in the same
        // shape, competing to be read as the name of the work.
        if let number = row.prNumber {
            text = row.isPullRequest ? "waiting on you" : "waiting on you · in PR #\(number)"
        } else if let branch = row.branch, !branch.isEmpty {
            // Finished on commits with no PR raised: say which branch, or the
            // row reads as "waiting on you" with nowhere to look.
            text = "no PR · on \(branch)"
        } else {
            text = "waiting on you"
        }
    case .ready:
        // The route rides the group header and the repo the leading token
        // (gh#125), so the sub-line keeps only what neither says: a routed
        // workspace whose name differs from the route's.
        let ws = (row.workspace != row.route ? row.workspace : nil) ?? ""
        if !row.dispatchable {
            text = ws.isEmpty ? noRouteLabel : "\(ws) · \(noRouteLabel)"
        } else {
            text = ws
        }
    case .done:
        // A row whose issue was deleted sits next to properly closed ones, and
        // the two are worth telling apart.
        let tail = row.gone ? "gone upstream" : (row.workspace.map { "ws:\($0)" } ?? "")
        text = [row.runtime, tail.isEmpty ? nil : tail]
            .compactMap { $0 }.filter { !$0.isEmpty }.joined(separator: " · ")
    }
    return BoardRowDetail(text: text, elapsed: elapsed, overCap: overCap,
                          billing: billingNote(row))
}

// MARK: - The detail sheet — a row is a door (gh#132)
//
// A board row shows a truncated title and nothing else you can open. The sheet
// is where the rest of it lives: the whole title, the issue body, the labels,
// where the work sits, what has been tried on it, and the links. Everything
// here is ported from `comet_proto::view::board` so the phone offers a row the
// same verbs the desktop panel and the TUI do — an action that exists on one
// surface and not another is a bug, not a platform difference.

/// The issue text behind a row, as `ReadBoardTask` answers.
///
/// Deliberately not a field on `TaskRow`: `WatchBoard` republishes every row on
/// every sync cycle, and a hundred issue bodies relayed to a phone to draw one
/// truncated line is the wrong trade. Read when a row is opened.
struct TaskDetail: Decodable {
    var id: String
    var body: String?
}

/// What the sheet says where the body would be, for an issue that has none. A
/// blank panel reads as a failed fetch; this reads as an empty issue.
let noBodyText = "No description on the issue."

/// What a detail fetch came back with.
///
/// Three states, not two: an issue with no description and a read that did not
/// happen are different facts, and a sheet that showed both as an empty body
/// would be lying about one of them. `nil` — no value at all — is still reading.
enum TaskBody: Equatable {
    case text(String)
    /// The issue has no description.
    case empty
    /// The read failed, with what the host (or the relay) said.
    case failed(String)
}

/// The row a detail sheet is open on, as `sheet(item:)` wants it: an identity,
/// not the row itself. The sheet re-reads the row off the board every frame, so
/// holding a copy here would be a second, staler board.
struct OpenedRow: Identifiable, Hashable {
    var id: String
    /// Skip the reading and land on the review (gh#256). Only the screenshot
    /// rig sets it — a tap always opens the row, and the review is a push from
    /// there.
    var openReview = false
}

/// One thing a surface offers to do with a row. Ported from `RowAction`.
enum BoardRowAction: Hashable {
    case dispatch
    case retry
    case cancel
    case openChat
    case openIssue
    case openPR

    /// The words every surface uses for it.
    var label: String {
        switch self {
        case .dispatch: return "Dispatch"
        case .retry: return "Retry"
        case .cancel: return "Cancel"
        case .openChat: return "Open chat"
        case .openIssue: return "Open issue"
        case .openPR: return "Open PR"
        }
    }

    /// SF Symbol, this surface's own — the vocabulary is shared, the glyphs
    /// are the platform's.
    var symbol: String {
        switch self {
        case .dispatch, .retry: return "play.fill"
        case .cancel: return "stop.circle"
        case .openChat: return "bubble.left.and.text.bubble.right"
        case .openIssue: return "arrow.up.right.square"
        case .openPR: return "arrow.triangle.pull"
        }
    }

    /// Does this end somebody's work?
    var destructive: Bool { self == .cancel }

    /// Does this release an agent, and so need the account picker first? The
    /// picker is not decoration — a dispatch spends somebody's subscription.
    var releases: Bool { self == .dispatch || self == .retry }
}

/// The actions a row's own affordances offer — the desktop chips, the TUI's
/// keys, this row's one chip. Ported from `row_actions`.
func boardRowActions(_ row: TaskRow) -> [BoardRowAction] {
    switch row.boardState {
    case .ready:
        return row.dispatchable ? [.dispatch] : []
    case .working:
        return [.openChat, .cancel]
    case .blocked:
        return (row.dispatchable ? [.retry] : []) + [.openChat, .cancel]
    case .review:
        return nonEmpty(row.prUrl) != nil ? [.openPR] : []
    case .failed:
        return (row.dispatchable ? [.retry] : []) + [.cancel]
    case .done:
        return []
    }
}

/// Everything the detail sheet offers: the row's own actions, plus the links a
/// list has no room for. Ported from `detail_actions`.
func boardDetailActions(_ row: TaskRow) -> [BoardRowAction] {
    var out = boardRowActions(row)
    if nonEmpty(row.prUrl) != nil, !out.contains(.openPR) { out.append(.openPR) }
    if !row.url.isEmpty { out.append(.openIssue) }
    return out
}

/// Is there anything on this row worth reviewing (gh#256, gh#344)?
///
/// Not a `BoardRowAction`: the actions are the shared rule `row_actions` owns,
/// and the review is a *screen* rather than something a row does — the desktop
/// reaches it through `shell::Route::Review`, not through a chip. This is the
/// phone's port of `comet_proto::view::board::reviewable`, exactly: an attempt
/// exists once `attempts > 0`, independently of the row's current state. That
/// includes a cancelled attempt returned to `ready`, and the historical
/// attempt while a retry is `working`; both still have a diff, claims and a
/// journal worth reading.
///
/// A pull request is enough on its own. A row whose pull request nobody
/// dispatched has no attempt and is still the work that most needs reading:
/// the diff is on GitHub, and with no claims the whole of it is unaccounted
/// for.
func boardReviewable(_ row: TaskRow) -> Bool {
    row.attempts > 0 || nonEmpty(row.prUrl) != nil
}

/// The detail sheet's navigation door. Kept as a named derivation so the
/// cross-language review runner checks the actual presentation decision, not
/// only the lower-level row rule it is built from.
func boardShowsReviewDoor(_ row: TaskRow) -> Bool {
    boardReviewable(row)
}

/// The URL an action opens, or nil for the ones that are not links.
func boardActionURL(_ row: TaskRow, _ action: BoardRowAction) -> URL? {
    switch action {
    case .openIssue: return nonEmpty(row.url).flatMap(URL.init(string:))
    case .openPR: return nonEmpty(row.prUrl).flatMap(URL.init(string:))
    default: return nil
    }
}

/// What has been tried on this row: `attempt 2 · last failed 3h ago · bills
/// brede@tally.no`. Ported from `history_line`.
///
/// Nil on a row nothing has ever run on: "attempt 0" is not a fact, it is a
/// blank where a fact would go.
func boardHistoryLine(_ row: TaskRow, now: Date = Date()) -> String? {
    guard row.attempts > 0 else { return nil }
    var parts = ["attempt \(row.attempts)"]
    if let outcome = nonEmpty(row.lastOutcome) {
        if let at = rfc3339(row.lastOutcomeAt) {
            parts.append("last \(outcome) \(formatAge(Int(now.timeIntervalSince(at)))) ago")
        } else {
            parts.append("last \(outcome)")
        }
    }
    if row.reopened > 0 { parts.append("reopened \(row.reopened)×") }
    // Whose subscription it spent, unconditionally — unlike the row's own
    // sub-line, which speaks up only when somebody else is paying. The sheet is
    // where you come to ask, and an answer that appears only when there is a
    // problem cannot be trusted to mean anything.
    if let billed = nonEmpty(row.billedTo) { parts.append(billsLabel(billed)) }
    return parts.joined(separator: " · ")
}

/// Where this row's work is happening: route, runtime, space, branch — each
/// named only when it is known. Ported from `placement_line`.
func boardPlacementLine(_ row: TaskRow) -> String? {
    var parts = [nonEmpty(row.route) ?? noRouteLabel]
    if let runtime = nonEmpty(row.runtime) { parts.append(runtime) }
    if let workspace = nonEmpty(row.workspace) { parts.append("ws:\(workspace)") }
    if let branch = nonEmpty(row.branch) { parts.append(branch) }
    return parts.isEmpty ? nil : parts.joined(separator: " · ")
}

// MARK: - Whose subscription a run spends (gh#101)

/// The phrase a `require-own` refusal carries, so a frontend can tell "the box
/// minds who pays for this" from every other reason a dispatch failed and offer
/// the confirm instead of a dead end. A shared constant on purpose — the
/// refusal is written by `comet-board` and matched here.
let requireOwnRefusal = "billing_guard = \"require-own\""

/// The email whose subscription a dispatch would spend, out of the logins the
/// host has saved. `slot` is the account id the dispatch names; nil is the
/// box's own CLI login, i.e. the *active* account for that harness.
///
/// Nil back means the device cannot name one. Nothing accuses anybody on the
/// strength of that — `crossBilled` is false whenever this is nil.
func billedEmail(accounts: [BoardAccount], harness: String?, slot: String?) -> String? {
    guard let harness, !harness.isEmpty else { return nil }
    let account: BoardAccount?
    if let slot, !slot.isEmpty {
        account = accounts.first { $0.harness == harness && $0.id == slot }
    } else {
        account = accounts.first { $0.harness == harness && $0.active }
    }
    guard let email = account?.email?.trimmingCharacters(in: .whitespaces),
          !email.isEmpty else { return nil }
    return email
}

/// Is this run spending somebody else's subscription? Claim-vs-slot-email and
/// nothing more: the dispatcher is what a frontend said, which the box cannot
/// verify — so two unknowns read as "not cross-billed", never as an accusation.
func crossBilled(billedTo: String?, dispatcher: String?) -> Bool {
    guard let billed = nonEmpty(billedTo), let by = nonEmpty(dispatcher) else { return false }
    return billed.caseInsensitiveCompare(by) != .orderedSame
}

/// What a picker row says about a selection that bills somebody else — the
/// short form, because it rides beside a chip already naming a login.
func billsLabel(_ billedTo: String) -> String {
    "bills \(billedTo.trimmingCharacters(in: .whitespaces))"
}

/// The one line a cross-billed release leads with. Names the subscription
/// (`Claude`, `Codex`) rather than the harness id, because the person about to
/// be charged thinks of it by the product's name.
func billsWarning(billedTo: String, harness: String?) -> String {
    "this run bills \(billedTo.trimmingCharacters(in: .whitespaces))'s \(subscriptionNoun(harness))"
}

/// The subscription a harness spends, named the way its owner would.
func subscriptionNoun(_ harness: String?) -> String {
    switch harness {
    case "claude-code": return "Claude"
    case "codex": return "Codex"
    case "cursor": return "Cursor"
    case "opencode": return "OpenCode"
    case "mock": return "mock"
    default: return "subscription"
    }
}

/// What a board row says about who it is charging, for the life of the attempt
/// — nil when nobody is being charged for somebody else. Derived from the row
/// alone, which is what lets the phone show it without asking the box anything.
func billingNote(_ row: TaskRow) -> String? {
    guard let billed = row.billedTo,
          crossBilled(billedTo: billed, dispatcher: row.dispatchedByUser) else { return nil }
    return billsLabel(billed)
}

private func nonEmpty(_ value: String?) -> String? {
    guard let trimmed = value?.trimmingCharacters(in: .whitespaces), !trimmed.isEmpty else {
        return nil
    }
    return trimmed
}

// MARK: - Live agents (gh#103, phone-shaped)

/// What a live attempt's agent is doing right now. Three states where the board
/// has two, and the split is the point: the board calls a dead run and an agent
/// asking a question both `blocked` — correctly, since both hold a chat and a
/// slot — but one wants an answer and the other a retry.
enum AgentState {
    case blocked, errored, working

    /// Lower is more urgent, and this is why blocked floats: a question
    /// outranks a corpse, which outranks work going fine on its own.
    var rank: Int {
        switch self {
        case .blocked: return 0
        case .errored: return 1
        case .working: return 2
        }
    }

    /// Worth interrupting a human for — what the section's count badge counts.
    var needsAttention: Bool { self != .working }

    /// The board's own glyphs, so a row means the same thing in the Agents
    /// section as it does on the board one tap away.
    var glyph: String {
        switch self {
        case .blocked: return BoardState.blocked.glyph
        case .errored: return BoardState.failed.glyph
        case .working: return BoardState.working.glyph
        }
    }

    var label: String {
        switch self {
        case .blocked: return "blocked"
        case .errored: return "errored"
        case .working: return "working"
        }
    }
}

/// One live attempt, as the Agents section draws it.
struct AgentRow: Identifiable, Hashable {
    var taskId: String
    /// Where the tap goes. Always a chat that exists — see `agentRows`.
    var chatId: String
    /// The issue identifier (`AGE-14`, `gh#103`): what the agent is *for*, and
    /// a better title than the chat's, which the agent writes about itself.
    var identifier: String
    /// A short slug of the task's title, drawn after the identifier (gh#364) —
    /// `gh#341 review-page-loads`. Its own field because it is decoration on
    /// the key and drops first: a row too narrow for both keeps the name.
    var slug: String?
    var branch: String?
    var state: AgentState
    /// The instant, not the age, so a view can re-read the clock on its own
    /// frames instead of rebuilding this list once a second.
    var startedAt: Date?
    var capSecs: Int?

    var id: String { chatId }

    func elapsedLabel(now: Date) -> String? {
        agentElapsedLabel(startedAt: startedAt, capSecs: capSecs, now: now)
    }

    func overCap(now: Date) -> Bool {
        agentOverCap(startedAt: startedAt, capSecs: capSecs, now: now)
    }
}

/// Every live attempt with a chat to open, most urgent first — a port of
/// `view::board::agent_rows`. The three inputs are the three standing streams
/// the app already holds: `WatchBoard` rows, the chat rows, the session mirror.
///
/// - A live attempt is `working` or `blocked` **with a chat id**. That is the
///   whole membership rule, and it is why a row leaves on its own: settle,
///   cancel and orphan all end the attempt, clearing `chat_id` and moving the
///   row out of both states in the same frame.
/// - The chat must exist here. A row whose chat has not synced (or is not
///   shared with this person) is dropped rather than drawn as something that
///   cannot be opened.
/// - State comes from the session mirror, not the row: the board's state is a
///   sync cycle old, and the mirror is live and staleness-gated, so a crashed
///   backend cannot leave an eternal spinner. The row's state is the fallback
///   for a chat with no session mirror yet.
func agentRows(rows: [TaskRow], chats: [Chat], sessions: [String: SessionRow],
               now: Date = Date()) -> [AgentRow] {
    let nowMillis = Int64(now.timeIntervalSince1970 * 1000)
    var out: [AgentRow] = rows.compactMap { row in
        guard row.boardState.holdsPane, let chatId = row.chatId,
              let chat = chats.first(where: { $0.id == chatId }) else { return nil }
        let branch = (chat.branch ?? row.branch)?.trimmingCharacters(in: .whitespaces)
        return AgentRow(
            taskId: row.id,
            chatId: chatId,
            identifier: row.identifier,
            slug: row.slug,
            // The chat's branch first: it is the checkout the agent is
            // actually in; the attempt row's copy is what it was cut as.
            branch: (branch?.isEmpty == false) ? branch : nil,
            state: agentState(row: row.boardState,
                              session: sessions[chatId], now: nowMillis),
            startedAt: row.startedAtDate,
            capSecs: row.maxDurationSecs)
    }
    // Urgency first, then longest-running — stable, since that order is start
    // order and start order never changes under a viewer. A row that cannot say
    // when it started sorts last; the identifier breaks the final tie.
    out.sort { a, b in
        if a.state.rank != b.state.rank { return a.state.rank < b.state.rank }
        if let x = a.startedAt, let y = b.startedAt, x != y { return x < y }
        if a.startedAt != nil && b.startedAt == nil { return true }
        if a.startedAt == nil && b.startedAt != nil { return false }
        return a.identifier < b.identifier
    }
    return out
}

/// What the session mirror says about this attempt, falling back to the board.
private func agentState(row: BoardState, session: SessionRow?, now: Int64) -> AgentState {
    switch effectiveStatus(session, now: now) {
    case .some(.working): return .working
    case .some(.awaitingInput): return .blocked
    case .some(.errored): return .errored
    // No live session: idle, stale, or never started. The board's verdict is
    // older but it is a verdict, and `blocked` is the one it reaches for a run
    // that ended without settling.
    case .some(.idle), .none: return row == .blocked ? .blocked : .working
    }
}

// MARK: - Unmanaged runs — every working chat the board is NOT running (gh#117)

/// What a chat with no title is called — `Chat.displayTitle`'s spelling, pinned
/// here so the Running group cannot drift from the sessions list a few rows
/// down.
let untitledChat = "New session"

/// One working chat that no board attempt accounts for — a port of
/// `view::board::RunningRow`.
///
/// Deliberately thinner than `AgentRow`: no issue, no branch promised, no cap
/// and no attempt behind it, so the row says only what is knowable.
struct RunningRow: Identifiable, Hashable {
    var chatId: String
    /// The chat's own title (`untitledChat` when it has none). The agent wrote
    /// it about itself, which is the best there is when no issue exists.
    var title: String
    /// `working` or `blocked` and never `errored` — membership is the live
    /// indicator, and an errored run is not a working one.
    var state: AgentState
    /// When the RUN started, off the session mirror — not when the chat was
    /// created, which for a long-lived chat is days ago.
    var startedAt: Date?

    var id: String { chatId }

    /// Bare elapsed: nothing caps these runs, so there is no second number to
    /// read it against.
    func elapsedLabel(now: Date) -> String? {
        agentElapsedLabel(startedAt: startedAt, capSecs: nil, now: now)
    }
}

/// Every working chat that is not a live board attempt, most urgent first — a
/// port of `view::board::running_rows`.
///
/// `agentRows` answers "what has the board released", which is a smaller
/// question than "what is working". A chat that raised in-chat subagents
/// instead of dispatching, an ad-hoc chat somebody started by hand, the chat
/// driving the board itself: real runs with no attempt row, and so nothing in
/// the Agents section at all.
///
/// - Membership is the session mirror and nothing else: `working` or
///   `awaitingInput`, staleness-gated, so the group fills within one watch
///   frame of a run starting and empties within one of it stopping.
/// - A live attempt is subtracted, not re-drawn: that chat belongs to the
///   Agents group, which knows its issue, branch and cap. The subtraction reads
///   the board rows rather than `agentRows`'s output, so a claimed chat stays
///   out even in the case that drops it from the other list.
/// - Archived is not a reason to hide a run. Archiving is a decision about a
///   *finished* chat; one working anyway is the invisible run this surfaces.
///
/// No board is required — `rows` empty (no board in the org, the sweep still
/// running, a phone that has not attached) subtracts nothing.
func runningRows(rows: [TaskRow], chats: [Chat], sessions: [String: SessionRow],
                 fallback: String? = nil, now: Date = Date()) -> [RunningRow] {
    let nowMillis = Int64(now.timeIntervalSince1970 * 1000)
    let dispatched = Set(rows.filter { $0.boardState.holdsPane }.compactMap(\.chatId))
    var out: [RunningRow] = chats.compactMap { chat in
        guard !dispatched.contains(chat.id) else { return nil }
        // The board's fallback chat has a fixed slot of its own (gh#122),
        // which carries its live state — a second row here would report the
        // same run twice.
        guard chat.id != fallback else { return nil }
        let session = sessions[chat.id]
        let state: AgentState
        switch effectiveStatus(session, now: nowMillis) {
        case .some(.working): state = .working
        case .some(.awaitingInput): state = .blocked
        // Errored and idle are not runs. A dead chat is the sessions list's to
        // report, at the recency it earned.
        default: return nil
        }
        let title = chat.title?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        return RunningRow(
            chatId: chat.id,
            title: title.isEmpty ? untitledChat : title,
            state: state,
            startedAt: session?.startedAt.map { Date(timeIntervalSince1970: Double($0) / 1000) })
    }
    // The same order the Agents group uses: a question outranks work going fine,
    // and under that longest-running first, which is stable because start order
    // never changes under a viewer. The chat id breaks the final tie — titles
    // are not unique and change under the reader as an agent renames its chat.
    out.sort { a, b in
        if a.state.rank != b.state.rank { return a.state.rank < b.state.rank }
        if let x = a.startedAt, let y = b.startedAt, x != y { return x < y }
        if a.startedAt != nil && b.startedAt == nil { return true }
        if a.startedAt == nil && b.startedAt != nil { return false }
        return a.chatId < b.chatId
    }
    return out
}

// MARK: - Active — the one group everything alive draws under (gh#123)

/// One row of the home screen's Active group — a port of
/// `view::board::ActiveRow`: a live board attempt, or a working chat no
/// attempt accounts for. The two memberships partition by construction
/// (`runningRows` subtracts every chat a live attempt claims), so the union
/// never draws a chat twice, and merging them is only a matter of order.
enum ActiveRow: Identifiable, Hashable {
    /// The board released this: it has an issue, a branch, a cap and a bill.
    case agent(AgentRow)
    /// A run the board never heard of: a chat driving the board, an ad-hoc
    /// one, anything started by hand.
    case unmanaged(RunningRow)

    /// Where the tap goes, and the row's identity — unique across both cases,
    /// because the partition never claims a chat twice.
    var chatId: String {
        switch self {
        case .agent(let row): return row.chatId
        case .unmanaged(let row): return row.chatId
        }
    }

    var id: String { chatId }

    var state: AgentState {
        switch self {
        case .agent(let row): return row.state
        case .unmanaged(let row): return row.state
        }
    }

    var startedAt: Date? {
        switch self {
        case .agent(let row): return row.startedAt
        case .unmanaged(let row): return row.startedAt
        }
    }
}

/// Everything alive, most urgent first, blind to how it started — a port of
/// `view::board::active_rows`.
///
/// The halves arrive pre-sorted, but concatenating them would put a working
/// attempt above a blocked hand-started run — the exact order the merge exists
/// to end — so the union sorts once, by the key both halves already use:
/// urgency, then longest-running, then the chat id, which every row carries
/// and no two rows share.
func activeRows(rows: [TaskRow], chats: [Chat], sessions: [String: SessionRow],
                fallback: String? = nil, now: Date = Date()) -> [ActiveRow] {
    var out = agentRows(rows: rows, chats: chats, sessions: sessions, now: now)
        .map(ActiveRow.agent)
        + runningRows(rows: rows, chats: chats, sessions: sessions,
                      fallback: fallback, now: now)
        .map(ActiveRow.unmanaged)
    out.sort { a, b in
        if a.state.rank != b.state.rank { return a.state.rank < b.state.rank }
        if let x = a.startedAt, let y = b.startedAt, x != y { return x < y }
        if a.startedAt != nil && b.startedAt == nil { return true }
        if a.startedAt == nil && b.startedAt != nil { return false }
        return a.chatId < b.chatId
    }
    return out
}

/// How many active rows want a human — the group header's count badge.
func activeNeedingAttention(_ rows: [ActiveRow]) -> Int {
    rows.filter { $0.state.needsAttention }.count
}

// MARK: - "Needs you" and the board-notices slot (gh#122)

/// One spelling everywhere, ports of `view::needs`'s constants: the product's
/// voice with three names is three voices.
let boardNoticesName = "Board notices"
let needsYouTitle = "Needs you"
/// The inbox's empty state, in words — a quiet check, never an omitted section.
let needsAllClear = "Nothing needs you"
/// What the slot says when nothing has been said there yet.
let boardNoticesNoReports = "No reports yet"

/// Why a stopped run stopped, when its harness could say (gh#545) — a port of
/// `comet_proto::StopReason`. Only the kinds the phone renders are spelled;
/// an unknown kind decodes as `.other` so a newer box cannot blank the inbox.
struct StopReason: Decodable, Hashable {
    /// Which company's window ran out, in the spelling a person reads —
    /// derived from the runtime, as the Rust derivation does.
    let kind: String
    let window: String?

    var isUsageLimit: Bool { kind == "usageLimit" }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        kind = try c.decodeIfPresent(String.self, forKey: .kind) ?? "other"
        window = try c.decodeIfPresent(String.self, forKey: .window)
    }

    enum CodingKeys: String, CodingKey {
        case kind, window
    }
}

/// Why a row is in the inbox — a port of `view::needs::NeedKind`.
enum NeedKind {
    /// Somebody is waiting on your answer: a question, or a permission prompt.
    case question
    /// The run stopped on a provider usage limit (gh#545): switch model,
    /// switch account, or wait — not plain retry.
    case limited
    /// A run died and is waiting on a retry.
    case deadRun
    /// The fallback chat finished a turn you have not seen.
    case report

    /// Inbox order — a question outranks a corpse, and both outrank news.
    var rank: Int {
        switch self {
        case .question: return 0
        case .limited: return 1
        case .deadRun: return 2
        case .report: return 3
        }
    }

    /// The board's own shape families: pointed is stuck, crossed is dead,
    /// checked is done-and-for-you.
    var glyph: String {
        switch self {
        case .question: return "▲"
        case .limited: return "◷"
        case .deadRun: return "✕"
        case .report: return "✓"
        }
    }

    /// What the row says when the chat has no last words to quote.
    var fallback: String {
        switch self {
        case .question: return "waiting on your answer"
        case .limited: return "hit its usage limit — switch model, switch account, or wait"
        case .deadRun: return "its run died — open to retry"
        case .report: return "finished a turn you haven't seen"
        }
    }
}

/// One thing waiting on a human: WHO, and one line of WHAT, in words — a port
/// of `view::needs::NeedRow`.
struct NeedRow: Identifiable, Hashable {
    /// Where the tap goes. Always a chat that exists here.
    var chatId: String
    var spaceId: String?
    /// `boardNoticesName`, an issue identifier (`gh#503`), or the chat's title.
    var who: String
    /// A slug of the task's title, after an identifier WHO (gh#364). Never set
    /// on the notices row or an ad-hoc chat's — those are named in words
    /// already, and a slug of a title beside the title says one thing twice.
    var slug: String?
    /// The chat's last words, or the kind's own words.
    var what: String
    var kind: NeedKind
    /// When this started waiting, best known.
    var since: Date?

    var id: String { chatId }
}

/// Model-written text collapsed onto one line — `view::single_line`.
private func singleLine(_ text: String?) -> String {
    (text ?? "").split(whereSeparator: \.isWhitespace).joined(separator: " ")
}

/// Everything waiting on a human, most owed first — a port of
/// `view::needs::needs_you`; membership and order are documented there. The
/// inputs are the four standing streams the app already holds: the fallback
/// address, `WatchBoard` rows, the chat rows, the session mirror.
func needsYou(fallback: String?, rows: [TaskRow], chats: [Chat],
              sessions: [String: SessionRow], now: Date = Date()) -> [NeedRow] {
    let nowMillis = Int64(now.timeIntervalSince1970 * 1000)
    var out: [NeedRow] = []

    func date(_ ms: Int64?) -> Date? {
        ms.map { Date(timeIntervalSince1970: Double($0) / 1000) }
    }
    func whatLine(_ chat: Chat, _ kind: NeedKind) -> String {
        let preview = singleLine(chat.lastMessagePreview)
        return preview.isEmpty ? kind.fallback : preview
    }
    // gh#545: which company's usage window this runtime spends. None for
    // anything unrecognised — "usage limit" alone is still true, and a wrong
    // name would be worse than none.
    func providerLabel(_ runtime: String?) -> String? {
        switch runtime ?? "" {
        case "claude-code", "claude": return "Claude"
        case "codex", "openai-codex": return "Codex"
        case "opencode": return "OpenCode"
        case "cursor": return "Cursor"
        default: return nil
        }
    }
    // The one line a usage-limited row gets, built from what the board knows
    // instead of quoting the transcript: `Claude 5-hour limit on
    // brede@tally.no — switch model, switch account, or wait`.
    func limitedLine(_ row: TaskRow) -> String {
        let stop = row.stopReason.flatMap { $0.isUsageLimit ? $0 : nil }
        let provider = providerLabel(row.runtime)
        let window = stop.flatMap { $0.window }
        let subject: String
        switch (provider, window) {
        case (.some(let p), .some(let w)): subject = "\(p) \(w) limit"
        case (.some(let p), nil): subject = "\(p) usage limit"
        case (nil, .some(let w)): subject = "\(w) usage limit"
        case (nil, nil): subject = "usage limit"
        }
        if let billed = row.billedTo?.trimmingCharacters(in: .whitespaces),
           !billed.isEmpty {
            return "\(subject) on \(billed) — switch model, switch account, or wait"
        }
        return "\(subject) — switch model, switch account, or wait"
    }

    // The fallback chat, by every door: question, dead run, unseen report.
    if let pin = fallback, let chat = chats.first(where: { $0.id == pin }) {
        let session = sessions[pin]
        let kind: NeedKind?
        switch chatIndicator(chat: chat, live: effectiveStatus(session, now: nowMillis)) {
        case .awaitingInput: kind = .question
        case .errored: kind = .deadRun
        case .completed: kind = .report
        case .working, .idle: kind = nil
        }
        if let kind {
            let since: Int64? = kind == .report
                ? chat.lastMessageAt
                : (session.map(\.updatedAt) ?? chat.lastMessageAt)
            out.append(NeedRow(chatId: chat.id, spaceId: chat.spaceId,
                               who: boardNoticesName, slug: nil, what: whatLine(chat, kind),
                               kind: kind, since: date(since)))
        }
    }

    // Live board attempts whose agent wants a human, named by their issue.
    for row in rows where row.boardState.holdsPane {
        guard let chatId = row.chatId, chatId != fallback,
              let chat = chats.first(where: { $0.id == chatId }) else { continue }
        let session = sessions[chatId]
        let kind: NeedKind
        switch agentState(row: row.boardState, session: session, now: nowMillis) {
        case .blocked: kind = row.stopReason?.isUsageLimit == true ? .limited : .question
        case .errored: kind = row.stopReason?.isUsageLimit == true ? .limited : .deadRun
        case .working: continue
        }
        // When the live session drove the verdict, its last transition is when
        // the waiting started; a board-only verdict knows the attempt's start.
        let live = effectiveStatus(session, now: nowMillis)
        let since: Date? = (live != nil && live != .idle)
            ? date(session?.updatedAt)
            : row.startedAtDate
        let what = kind == .limited ? limitedLine(row) : whatLine(chat, kind)
        out.append(NeedRow(chatId: chatId, spaceId: chat.spaceId,
                           who: row.identifier, slug: row.slug, what: what,
                           kind: kind, since: since))
    }

    // Everything else that is asking. Archived is not a reason to hide a
    // question — the rule the Running group already keeps.
    let claimed = Set(rows.filter { $0.boardState.holdsPane }.compactMap(\.chatId))
    for chat in chats where chat.id != fallback && !claimed.contains(chat.id) {
        let session = sessions[chat.id]
        guard effectiveStatus(session, now: nowMillis) == .awaitingInput else { continue }
        out.append(NeedRow(chatId: chat.id, spaceId: chat.spaceId,
                           who: chat.displayTitle, slug: nil, what: whatLine(chat, .question),
                           kind: .question, since: date(session?.updatedAt)))
    }

    out.sort { a, b in
        if a.kind.rank != b.kind.rank { return a.kind.rank < b.kind.rank }
        if let x = a.since, let y = b.since, x != y { return x < y }
        if a.since != nil && b.since == nil { return true }
        if a.since == nil && b.since != nil { return false }
        return a.chatId < b.chatId
    }
    return out
}

/// The board's fallback chat as a pinned thread — a port of
/// `view::needs::FallbackSlot`.
struct BoardNoticesSlot: Hashable {
    /// That chat. Opening it marks it seen — the synced marker that clears
    /// `unseen` on every device.
    var chatId: String
    var spaceId: String?
    /// The latest report, one line. `nil` = never spoke, and the slot says
    /// `boardNoticesNoReports` instead of vanishing.
    var preview: String?
    var unseen: Bool
    /// Live status, so a turn running now can never be mistaken for an
    /// 8h-old report.
    var indicator: ChatIndicator
    /// When it last spoke (epoch ms).
    var lastAt: Int64?
}

/// The slot, or `nil` when the board has no fallback chat or that chat has not
/// synced here. A set-but-silent chat is NOT `nil`: that is the empty fixture,
/// and it renders.
func boardNoticesSlot(fallback: String?, chats: [Chat],
                      sessions: [String: SessionRow],
                      now: Date = Date()) -> BoardNoticesSlot? {
    guard let pin = fallback, let chat = chats.first(where: { $0.id == pin }) else {
        return nil
    }
    let nowMillis = Int64(now.timeIntervalSince1970 * 1000)
    let preview = singleLine(chat.lastMessagePreview)
    return BoardNoticesSlot(
        chatId: chat.id,
        spaceId: chat.spaceId,
        preview: preview.isEmpty ? nil : preview,
        unseen: chat.unseen,
        indicator: chatIndicator(chat: chat,
                                 live: effectiveStatus(sessions[pin], now: nowMillis)),
        lastAt: chat.lastMessageAt)
}

// MARK: - Which device hosts the board (gh#55)

/// The devices to try, in order, when the operator has named none.
///
/// The desktop's `host_candidates` leads with `None` — itself, because a local
/// board must win over a remote one. The phone has no engine and therefore no
/// local board, so that entry is dropped and every candidate is a real device
/// room to dial. iOS rows are excluded for the same reason: mobile is a
/// controller, and `WorkspaceStore` purges those rows anyway.
///
/// A candidate is ruled out by its `WatchBoard` stream ending without ever
/// delivering a frame — the engine refuses the subscription outright when it
/// hosts no board, so "said nothing at all" IS the answer. Registration order
/// (createdAt, id tiebreak) is stable across heartbeats, so the sweep visits
/// them the same way twice.
func boardHostCandidates(_ devices: [DeviceRow]) -> [String] {
    devices
        .filter { $0.platform != "ios" }
        .sorted { a, b in
            let (ca, cb) = (a.createdAt ?? 0, b.createdAt ?? 0)
            return ca == cb ? a.id < b.id : ca < cb
        }
        .map(\.id)
}
