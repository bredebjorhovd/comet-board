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

    enum CodingKeys: String, CodingKey {
        case id, identifier, title, state, source, url, labels, dispatchable, gone
        case route, workspace, runtime, branch, attempts, reopened, account
        case chatId = "chat_id"
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
    }

    /// Memberwise init for demo rows and tests (the decoder owns the wire).
    init(id: String, identifier: String, title: String, state: BoardState,
         source: String = "github", url: String = "", labels: [String] = [],
         dispatchable: Bool = true, gone: Bool = false, route: String? = nil,
         workspace: String? = nil, runtime: String? = nil, chatId: String? = nil,
         prUrl: String? = nil, prNumber: Int? = nil, branch: String? = nil,
         dispatchedBy: String? = nil, dispatchedByChat: String? = nil,
         lastOutcome: String? = nil, lastOutcomeAt: String? = nil,
         attempts: Int = 0, reopened: Int = 0, updatedAt: String = "",
         startedAt: String? = nil, account: String? = nil,
         dispatchedByUser: String? = nil, billedTo: String? = nil,
         maxDurationSecs: Int? = nil) {
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
    }

    var boardState: BoardState { BoardState.parse(state) }

    /// The live attempt's start instant, when the row carries one.
    var startedAtDate: Date? { rfc3339(startedAt) }
}

/// One runtime a dispatch can be pointed at, as `ListBoardRuntimes` reports it.
/// `name` is exactly what the `DispatchTask` override accepts; `harness` is how
/// the account picker knows which saved logins a runtime could spend (gh#74).
struct BoardRuntime: Decodable, Hashable, Identifiable {
    var name: String
    var label: String
    var harness: String

    var id: String { name }
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
        if let number = row.prNumber {
            text = "PR #\(number) · waiting on you"
        } else if let branch = row.branch, !branch.isEmpty {
            // Finished on commits with no PR raised: say which branch, or the
            // row reads as "waiting on you" with nowhere to look.
            text = "\(branch) · no PR"
        } else {
            text = "waiting on you"
        }
    case .ready:
        // Where it would go, not where it came from: the routed workspace is
        // the routing outcome, otherwise invisible until dispatch.
        let repo = row.workspace ?? row.route ?? ""
        if !row.dispatchable {
            text = repo.isEmpty ? noRouteLabel : "\(repo) · \(noRouteLabel)"
        } else {
            text = repo
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

/// How many live agents want a human — the section header's count badge.
func agentsNeedingAttention(_ rows: [AgentRow]) -> Int {
    rows.filter { $0.state.needsAttention }.count
}

// MARK: - Which device hosts the board (gh#55)

/// The devices to try, in order, when the operator has pinned none.
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
