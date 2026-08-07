//! Board vocabulary shared by every surface: the state enum with its section
//! order and glyphs, the `TaskRow` wire shape `WatchBoard` streams, and the
//! view derivations (sections, the `f`/`/` filter cycle, what each row says)
//! that both viewports draw from.
//!
//! This lives in proto, not in `comet-board`, for the same reason the rest of
//! [`crate::view`] does: the viewports (`comet-tui` today, the gpui app later)
//! render board rows without depending on the board crate, and a glyph, section
//! order or filter position that differs between surfaces is a real bug.
//! `comet-board` re-exports `BoardState` and `TaskRow`, so board-side code and
//! its tests are unchanged.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Board-level task state. Note the deliberate divergence from herdr's
/// vocabulary: herdr's `done` means "agent finished, you haven't looked", which
/// is our `review`. Our `done` means the issue is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BoardState {
    Blocked,
    Working,
    Ready,
    Review,
    Failed,
    Done,
}

impl BoardState {
    /// Fixed section order on the board: blocked → working → ready → review →
    /// failed → done.
    pub const SECTION_ORDER: [BoardState; 6] = [
        BoardState::Blocked,
        BoardState::Working,
        BoardState::Ready,
        BoardState::Review,
        BoardState::Failed,
        BoardState::Done,
    ];

    /// Shape-distinct glyph per state. Three shape families on purpose —
    /// pointed (`▲ ▸`), round (`● ·`), crossed (`✓ ✕`) — so every state
    /// survives color being stripped.
    pub fn glyph(self) -> &'static str {
        match self {
            BoardState::Blocked => "▲",
            BoardState::Working => "●",
            BoardState::Ready => "▸",
            BoardState::Review => "✓",
            BoardState::Failed => "✕",
            BoardState::Done => "·",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            BoardState::Blocked => "BLOCKED",
            BoardState::Working => "WORKING",
            BoardState::Ready => "READY",
            BoardState::Review => "REVIEW",
            BoardState::Failed => "FAILED",
            BoardState::Done => "DONE",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            BoardState::Blocked => "blocked",
            BoardState::Working => "working",
            BoardState::Ready => "ready",
            BoardState::Review => "review",
            BoardState::Failed => "failed",
            BoardState::Done => "done",
        }
    }

    pub fn parse(s: &str) -> Option<BoardState> {
        Some(match s {
            "blocked" => BoardState::Blocked,
            "working" => BoardState::Working,
            "ready" => BoardState::Ready,
            "review" => BoardState::Review,
            "failed" => BoardState::Failed,
            "done" => BoardState::Done,
            _ => return None,
        })
    }

    /// A task holding a pane. `blocked` counts — it still occupies a pane, so it
    /// counts against `max_concurrent_per_workspace`. (Board policy, kept on
    /// the enum so the move here changed no call sites.)
    pub fn holds_pane(self) -> bool {
        matches!(self, BoardState::Working | BoardState::Blocked)
    }

    /// Finished for good, with no retry left to come.
    ///
    /// Only `done` qualifies: `review` is waiting for you and `failed` is
    /// waiting for a retry, and both of those still have a use for the attempt's
    /// worktree — a retry reuses the checkout already holding the branch. This
    /// is the filter `gc` prunes by.
    pub fn is_terminal(self) -> bool {
        matches!(self, BoardState::Done)
    }
}

impl fmt::Display for BoardState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One runtime a dispatch can be pointed at, as `ListBoardRuntimes` reports it.
///
/// `name` is exactly what `routing.toml` and the `DispatchTask` override
/// accept; `label` is the human spelling a picker shows; `harness` is what the
/// board resolves the name to, which is how an account picker knows which saved
/// logins a runtime could spend (gh#74).
///
/// Here rather than in `comet-board` for the same reason [`TaskRow`] is: both
/// viewports deserialize it, and neither depends on the board crate.
/// `comet_board::runtime` re-exports it and owns what the list contains.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeOption {
    pub name: String,
    pub label: String,
    pub harness: crate::HarnessId,
}

/// The mark a pinned orchestrator's session row carries, on both viewports.
///
/// Shape-distinct from every [`BoardState::glyph`] and from the session dot, on
/// the same rule those follow: it has to survive colour being stripped, because
/// one row in the list meaning something different from all the others is the
/// whole point of drawing it.
pub const ORCHESTRATOR_GLYPH: &str = "◆";

/// Which chat, if any, is pinned as this board's orchestrator (gh#104).
///
/// A frame of its own rather than a field on [`TaskRow`], and a *stream* rather
/// than a read, for the same reason: the pin is a property of the board and not
/// of any task on it, and every surface that renders it — the sidebar, on both
/// viewports — needs it before a board panel has ever been opened. Reading it
/// off `ReadBoardConfig` would work and would cost a git probe per space every
/// time, which is the wrong price for a glyph.
///
/// A struct rather than a bare `Option<String>` because this is the frame a
/// pinned-chat feature grows in: `null` today means unpinned, and a field added
/// beside it later does not change what an old client already parses.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrchestratorPin {
    /// The pinned chat's id. `None` = this board has no orchestrator, which is
    /// the default and a legitimate way to run one.
    #[serde(default)]
    pub chat_id: Option<String>,
}

/// One task, in the shape callers are promised: herdr-board's `list --json`
/// contract with the pane→chat rename applied.
///
/// This shape is a published contract, consumed three ways: `WatchBoard`
/// streams it, the `comet-board` CLI prints it (H6, verbatim), and the agent
/// conventions text teaches orchestrating agents to poll it. Field renames from
/// herdr-board are exactly the two the port dictates — `pane_id` → `chat_id`,
/// `dispatched_by_pane` → `dispatched_by_chat` — because the values *are* chat
/// ids now, and a contract that lies about what its ids address is worse than
/// one that renames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRow {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub state: String,
    pub source: String,
    pub url: String,
    pub labels: Vec<String>,
    /// False when no route matches, or when the issue is gone upstream: the
    /// task is on the board but cannot be dispatched, and `dispatch` refuses it.
    pub dispatchable: bool,
    /// The issue behind this row no longer exists upstream. The row is kept for
    /// the attempts on it; there is nothing left to work on.
    pub gone: bool,
    pub route: Option<String>,
    /// The comet space (herdr's workspace — the config key keeps that name).
    pub workspace: Option<String>,
    pub runtime: Option<String>,
    /// The live attempt's chat (herdr-board's `pane_id`).
    pub chat_id: Option<String>,
    /// Set on `review` rows, which is how a PR reaches an orchestrator.
    pub pr_url: Option<String>,
    pub pr_number: Option<i64>,
    pub branch: Option<String>,
    /// Parent task id when the board dispatched the releasing agent too. Null
    /// on its own does **not** mean the operator: an orchestrating chat has no
    /// attempt and so no task id — read `dispatched_by_chat` as well.
    pub dispatched_by: Option<String>,
    /// The chat the dispatch ran from, when an agent was in it (herdr-board's
    /// `dispatched_by_pane`). Set for every agent-released row, and the address
    /// a parent can be reached at. Both this and `dispatched_by` null means the
    /// operator released it.
    pub dispatched_by_chat: Option<String>,
    /// How the most recent *ended* attempt ended: `done`, `failed`, `cancelled`
    /// or `orphaned`. Null means no attempt has ever ended.
    ///
    /// This is what makes cancellation legible to a parent agent. `cancelled`
    /// derives back to `ready` — deliberately, since the issue is still owed —
    /// so without this field a child the operator killed is indistinguishable
    /// from one that was never dispatched. Set even while a *newer* attempt is
    /// live, so a retry does not erase how the previous one went.
    pub last_outcome: Option<String>,
    /// When that attempt ended (RFC 3339). Pairs with `last_outcome` so a
    /// long-lived poller can tell a fresh cancellation from one it already saw.
    pub last_outcome_at: Option<String>,
    pub attempts: usize,
    /// How many times the board closed this row's attempt and then found its
    /// agent still working (herdr-board gh#34's honesty field).
    pub reopened: i64,
    /// When the issue last changed upstream (RFC 3339). Missing on a row that
    /// predates this field; empty string and `None` mean the same thing.
    ///
    /// Fork addition to herdr-board's contract, which never needed it — its
    /// view read the internal task. The `done` section is bounded to today here,
    /// and a viewport that cannot tell today's close from history cannot draw
    /// that section, so the wire carries the timestamp instead of two viewports
    /// guessing at one.
    #[serde(default)]
    pub updated_at: String,
    /// When the live attempt started (RFC 3339), for the elapsed counter on a
    /// working or blocked row. `None` when nothing is running.
    #[serde(default)]
    pub started_at: Option<String>,
    /// The agent account whose subscription this row's attempt spends — the
    /// slot id the attempt recorded, falling back to the route's default for a
    /// row nothing has run on yet. `None` is the device's own CLI login, which
    /// is also every row on a single-account box (gh#59).
    #[serde(default)]
    pub account: Option<String>,
    /// The human whose frontend released this row's attempt, as that frontend
    /// named them — an email when it knows one, else the user id (gh#74). Null
    /// where nobody said: a `comet-board` dispatch, an agent's, and every
    /// attempt from before the frontends sent it.
    ///
    /// A claim, not a credential. Board calls relay as the device room's owner,
    /// so the box cannot check who really pressed enter; #66's identity work is
    /// what will make this checkable. Read it as "who says they did", and never
    /// as permission to do anything.
    #[serde(default)]
    pub dispatched_by_user: Option<String>,
    /// Whose subscription this row's attempt actually spends, as an email
    /// (gh#101): the [`account`](Self::account) slot's login, or the box's own
    /// CLI login when the dispatch named no slot.
    ///
    /// Resolved once, at dispatch, and recorded on the attempt — the slot id in
    /// `account` means nothing to a reader who has not saved that login, and
    /// the box's own login can change under a run that is still going. `None`
    /// on a row nothing has run on yet, and on attempts from before this
    /// existed.
    ///
    /// Compared against [`dispatched_by_user`](Self::dispatched_by_user), this
    /// is what makes a cross-billed run visible for its whole life — see
    /// [`cross_billed`].
    #[serde(default)]
    pub billed_to: Option<String>,
    /// The wall-clock cap one attempt on this row gets (gh#70's `max_duration`,
    /// in seconds), resolved route-then-defaults. `None` is uncapped.
    ///
    /// On the wire because the *elapsed* counter is worth nothing without it:
    /// "1h50m" says one thing under a two-hour cap and another under six, and
    /// only the board knows which — the routing config is the host's, and a
    /// viewport reading a relayed board has never seen it (gh#103).
    #[serde(default)]
    pub max_duration_secs: Option<u64>,
}

// ---------------------------------------------------------------------------
// View derivations
// ---------------------------------------------------------------------------
//
// Everything both viewports need to say the same thing about the same board,
// ported from herdr-board's `ui/state.rs` and `ui/render.rs`. The TUI renders
// today; the gpui app grows the same view later. A row's state, its section,
// its metadata and the filter that hides it are all derived here, once.

use chrono::{DateTime, Utc};

/// What the route column renders for a row nothing routes — and so what the
/// header says, and what a typed query has to match.
pub const NO_ROUTE: &str = "no route";

impl TaskRow {
    /// The board state this row carries. An unrecognized wire value defaults to
    /// `ready` — a schema-skewed row must read as waiting, not crash the view.
    pub fn state(&self) -> BoardState {
        BoardState::parse(&self.state).unwrap_or(BoardState::Ready)
    }

    /// The leading token a board row shows: the CLI's repo-qualified form,
    /// humanized — `tally #507`, not `gh#507` (gh#125).
    ///
    /// GitHub numbers issues per repository, so `gh#507` and `gh#44` can be
    /// different repos distinguishable only by a muted sub-line. The repo name
    /// is the half of the id that makes the identifier readable on its own; the
    /// owner stays out for the same reason it stays out of branch names. A
    /// Linear identifier (`LIN-142`) is already unique across the board and is
    /// shown unchanged, as is any id this rule cannot parse.
    pub fn display_identifier(&self) -> String {
        let number = self
            .id
            .rsplit(['#', '!'])
            .next()
            .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        match (gh_repo_name(&self.id), number) {
            (Some(repo), Some(n)) if !repo.is_empty() => format!("{repo} #{n}"),
            _ => self.identifier.clone(),
        }
    }
}

/// The `owner/repo` a GitHub task id names — `gh:Florin-AS/tally#507` →
/// `Florin-AS/tally`. `None` for a Linear id, which names no repo.
///
/// Lives here (and is re-exported by `comet_board::model`) because both the
/// board crate and every viewport read it: the board keys branches and panes on
/// it, the viewports key the row's leading token on it, and two parsers of one
/// id format is one too many.
pub fn gh_repo(task_id: &str) -> Option<&str> {
    // `!` is the pull-request form of the id: `gh:owner/repo!508`.
    task_id.strip_prefix("gh:")?.split(['#', '!']).next()
}

/// Just the repository's name — `Florin-AS/tally` → `tally`.
///
/// The owner is noise when you work with a handful of repos; the name is the
/// part you read, and so the part that names branches, panes and board rows.
pub fn gh_repo_name(task_id: &str) -> Option<&str> {
    gh_repo(task_id)?.rsplit('/').next()
}

/// Showing less than everything.
///
/// **A filter is a view, and changes nothing else.** Not what syncs, not what
/// is dispatchable, not what counts against `max_concurrent_per_workspace` — a
/// hidden row is still a live attempt with an agent working it. It lives here
/// so both viewports implement the `f`/`/` cycle the same way.
///
/// One filter at a time. `f` and `/` answer different questions — "show me one
/// project" and "where was that ticket" — and a board obeying both answers
/// neither.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Filter {
    #[default]
    All,
    /// One route, cycled with `f`.
    Route(String),
    /// The rows nothing routes, cycled with `f` after the named routes.
    ///
    /// Not a missing filter but a real one: a route that ANDs a repo with a
    /// label leaves most of that repo's backlog marked `no route`, on the board
    /// and deliberately undispatchable. Without a position of their own they
    /// are the rows you most need to see as a group and the only ones you
    /// cannot filter to — or away from.
    NoRoute,
    /// A substring of identifier, title or route, typed at `/`.
    Text(String),
}

impl Filter {
    pub fn active(&self) -> bool {
        !matches!(self, Filter::All)
    }

    pub fn matches(&self, row: &TaskRow) -> bool {
        match self {
            Filter::All => true,
            Filter::Route(r) => row.route.as_deref() == Some(r.as_str()),
            // The same field the named routes partition on, so every row is in
            // exactly one position of the cycle.
            Filter::NoRoute => row.route.is_none(),
            Filter::Text(q) => {
                let q = q.trim();
                q.is_empty()
                    || contains(&row.identifier, q)
                    // The row renders `tally #507`, so `/tally` must reach it —
                    // matching what is rendered is the least surprising rule.
                    || contains(&row.display_identifier(), q)
                    || contains(&row.title, q)
                    || row.route.as_deref().is_some_and(|r| contains(r, q))
                    // The route column renders `no route` on such a row, and
                    // matching what is rendered is the least surprising rule —
                    // it also makes `/no route` reach the group without having
                    // to know `f` exists at all.
                    || (row.route.is_none() && contains(NO_ROUTE, q))
            }
        }
    }

    /// What the header says is active. A filtered board that does not say so is
    /// a board that looks broken.
    pub fn label(&self) -> Option<String> {
        match self {
            Filter::All => None,
            Filter::Route(r) => Some(format!("filter: {r}")),
            // Said the way any other position is said, and in the words the
            // rows themselves use.
            Filter::NoRoute => Some(format!("filter: {NO_ROUTE}")),
            Filter::Text(q) => Some(format!("/{q}")),
        }
    }

    /// What to say when the filter has hidden every row, which otherwise draws
    /// a body with nothing in it at all.
    pub fn empty_note(&self) -> Option<String> {
        match self {
            Filter::Text(q) if q.trim().is_empty() => None,
            Filter::All => None,
            Filter::Route(r) => Some(format!(
                "Nothing on the board routes to {r}.  f moves on, F clears the filter."
            )),
            // Reachable when the last unrouted row leaves under the filter —
            // `f` never offers this position on a board where everything routes.
            Filter::NoRoute => Some(
                "Everything on the board has a route.  f moves on, F clears the filter."
                    .to_string(),
            ),
            Filter::Text(q) => Some(format!(
                "No identifier, title or route matches `{q}`.  F clears the filter."
            )),
        }
    }
}

fn contains(hay: &str, needle: &str) -> bool {
    hay.to_lowercase().contains(&needle.to_lowercase())
}

/// Every route with a row on the board, alphabetically.
///
/// Alphabetical rather than in board order: `f` is a tour of the projects, and
/// a tour whose order changes as rows move between sections is not one you can
/// learn. Only routes with a row that is *on* the board — offering a route
/// whose only rows are yesterday's `done` would filter to nothing.
pub fn routes_present(rows: &[TaskRow], now: DateTime<Utc>) -> Vec<String> {
    let mut out: Vec<String> = on_board(rows, now)
        .filter_map(|row| row.route.clone())
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// The rows `f` is a tour of: everything except history older than today.
///
/// Whether a position is offered and what it then shows have to be decided from
/// the same rows, or `f` offers stops that filter to nothing.
fn on_board(rows: &[TaskRow], now: DateTime<Utc>) -> impl Iterator<Item = &TaskRow> {
    rows.iter()
        .filter(move |row| row.state() != BoardState::Done || finished_today(row, now))
}

/// The positions `f` steps through, in order. `all` is the wrap, not a
/// position.
///
/// `no route` comes last, after the named routes: it is the odd one out, and
/// putting it at the end leaves the routes in a stable order as they come and
/// go rather than shifting every one of them by a place.
///
/// A position with no rows is never offered — an empty one is a keypress that
/// appears to do nothing, which reads as a bug. That is why a route whose only
/// row closed yesterday is left out, and it is the same reason `no route` is
/// skipped on a board where everything routes.
pub fn filter_cycle(rows: &[TaskRow], now: DateTime<Utc>) -> Vec<Filter> {
    let mut out: Vec<Filter> = routes_present(rows, now).into_iter().map(Filter::Route).collect();
    if on_board(rows, now).any(|row| row.route.is_none()) {
        out.push(Filter::NoRoute);
    }
    out
}

/// Sections in fixed order, empty ones omitted entirely.
///
/// `done` is bounded to today. The header says "DONE today" and it means it:
/// every issue ever closed in a tracked repo derives to `done`, and ninety-odd
/// of them is a wall of history, not a board.
///
/// The filter is applied **here**, so everything downstream — the rows, the
/// header counts, which sections exist at all — agrees on what is shown
/// without each having to remember to ask. A section whose rows are all
/// filtered away is empty, and an empty section is already omitted.
pub fn sections<'a>(
    rows: &'a [TaskRow],
    filter: &Filter,
    now: DateTime<Utc>,
) -> Vec<(BoardState, Vec<&'a TaskRow>)> {
    BoardState::SECTION_ORDER
        .iter()
        .filter_map(|&state| {
            let section: Vec<&TaskRow> = rows
                .iter()
                .filter(|row| row.state() == state)
                .filter(|row| state != BoardState::Done || finished_today(row, now))
                .filter(|row| filter.matches(row))
                .collect();
            if section.is_empty() {
                None
            } else {
                Some((state, section))
            }
        })
        .collect()
}

/// One route's rows inside a section — the unit a hundred-row board is scanned
/// by (gh#125).
#[derive(Debug, PartialEq)]
pub struct SectionGroup<'a> {
    /// The route the group collects, keyed on the same field [`Filter::Route`]
    /// and the `f` cycle partition on. `None` is the `no route` group.
    pub route: Option<String>,
    pub rows: Vec<&'a TaskRow>,
}

impl SectionGroup<'_> {
    /// What the group header says: the route's name, or [`NO_ROUTE`] — the
    /// words the rows themselves use.
    pub fn label(&self) -> &str {
        self.route.as_deref().unwrap_or(NO_ROUTE)
    }
}

/// Whether a group starts folded, absent an operator's own toggle.
///
/// Unrouted rows are visibility-only by design — on the board, deliberately
/// undispatchable — so their group starts folded: worth a headline and a
/// count, never pole position over rows an `enter` can actually release. But
/// only on the unfiltered board: a filter is the operator asking for specific
/// rows, and a default fold that hides what was just asked for (`f` to the
/// `no route` position, `/` matching an unrouted title) fights the ask.
pub fn group_starts_collapsed(filter: &Filter, route: Option<&str>) -> bool {
    matches!(filter, Filter::All) && route.is_none()
}

/// [`sections`], with each section's rows grouped by route.
///
/// A flat section of a hundred rows cannot be scanned; the same rows as
/// "tally 34 · herdr-board 12 · …" can. Biggest group first — the summary reads
/// as a ranking — with ties alphabetical so equal groups do not trade places
/// between frames, and rows in board order within each group. `no route` sits
/// last regardless of size: it must never hold the top of a section, which is
/// the first selected row of the whole panel.
pub fn grouped_sections<'a>(
    rows: &'a [TaskRow],
    filter: &Filter,
    now: DateTime<Utc>,
) -> Vec<(BoardState, Vec<SectionGroup<'a>>)> {
    sections(rows, filter, now)
        .into_iter()
        .map(|(state, rows)| {
            let mut groups: Vec<SectionGroup<'a>> = Vec::new();
            for row in rows {
                match groups.iter_mut().find(|g| g.route == row.route) {
                    Some(group) => group.rows.push(row),
                    None => groups.push(SectionGroup {
                        route: row.route.clone(),
                        rows: vec![row],
                    }),
                }
            }
            groups.sort_by(|a, b| {
                a.route
                    .is_none()
                    .cmp(&b.route.is_none())
                    .then(b.rows.len().cmp(&a.rows.len()))
                    .then_with(|| a.route.cmp(&b.route))
            });
            (state, groups)
        })
        .collect()
}

/// Whether a section draws group headers at all.
///
/// One routed group needs none — three WORKING rows from one repo are readable
/// bare, and a header repeating what every row's leading token says is noise.
/// On the unfiltered board a lone `no route` group still draws its header: the
/// header is what keeps those rows folded, and folded rows need a headline to
/// be findable. Under a filter that lone header would only repeat what the
/// filter chip already says.
pub fn group_headers_shown(filter: &Filter, groups: &[SectionGroup]) -> bool {
    groups.len() > 1
        || (matches!(filter, Filter::All)
            && groups.first().is_some_and(|g| g.route.is_none()))
}

/// Was this task closed today, in the operator's own timezone?
///
/// Local midnight, not a rolling 24 hours: "today" is a thing a person means,
/// and a row dropping off at 14:37 because that is when it closed yesterday
/// would be baffling.
pub fn finished_today(row: &TaskRow, now: DateTime<Utc>) -> bool {
    let Ok(updated) = DateTime::parse_from_rfc3339(&row.updated_at) else {
        // An unparseable timestamp is not evidence of recency.
        return false;
    };
    let local = updated.with_timezone(&chrono::Local);
    local.date_naive() == now.with_timezone(&chrono::Local).date_naive()
}

/// `12s` / `9m04s` / `1h20m`. Minute resolution was rejected: a counter that
/// never visibly moves is not worth the redraw.
pub fn format_elapsed(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

/// A *cap* said the way a person configured it: `2h`, `45m`, `1h30m`.
///
/// Deliberately not [`format_elapsed`]: a cap is a round number somebody typed
/// into `routing.toml`, and rendering `defaults.max_duration = "2h"` as `2h00m`
/// makes the reader check whether the minutes mean anything. They never do.
pub fn format_cap(secs: u64) -> String {
    if secs < 60 {
        return format!("{secs}s");
    }
    let (h, m) = (secs / 3600, (secs % 3600) / 60);
    match (h, m) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h{m:02}m"),
    }
}

/// Coarser form for the header (`synced 12s`, `last synced 4m`).
pub fn format_age(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h", s / 3600)
    }
}

// ---- what rows say -------------------------------------------------------

/// Below this width, drop all metadata rather than wrap.
pub const NARROW_LIMIT: u16 = 60;

/// The right-hand metadata block for a row, already collapsed for `width`.
///
/// Below 60 columns everything goes rather than wrapping. Above it the block
/// is one line of fixed-width fields, so it cannot crowd the title.
///
/// herdr-board collapsed this block right-to-left (elapsed survives longest),
/// which only ever mattered because its `idle` marker widened the elapsed
/// field. comet's wire carries no agent-status "settled but quiet" state, so
/// elapsed stays short and the full block always fits at the widths that show
/// metadata at all — the collapse has nothing to collapse and is gone.
pub fn row_metadata(row: &TaskRow, selected: bool, width: u16, now: DateTime<Utc>) -> String {
    if width < NARROW_LIMIT {
        return String::new();
    }
    // Whose subscription this row is spending, when it is not the releaser's
    // (gh#101). Appended rather than folded into the per-state arms: it is true
    // of an attempt for its whole life — working, blocked, in review, long
    // closed — and a fact that survives the row changing section does not
    // belong inside the match on which section it is in.
    let base = state_metadata(row, selected, now);
    match billing_note(row) {
        Some(note) if base.trim().is_empty() => note,
        Some(note) => format!("{} · {note}", base.trim_end()),
        None => base,
    }
}

fn state_metadata(row: &TaskRow, selected: bool, now: DateTime<Utc>) -> String {
    match row.state() {
        BoardState::Working | BoardState::Blocked => {
            let elapsed = row
                .started_at
                .as_deref()
                .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
                // Never negative: a clock skew must not render as a count-up
                // from the future.
                .map(|start| {
                    format_elapsed((now - start.with_timezone(&Utc)).num_seconds().max(0))
                })
                .unwrap_or_default();
            format!(
                "{}{}{}",
                fixed(row.runtime.as_deref().unwrap_or(""), 12),
                fixed(&ws(row), 11),
                elapsed
            )
        }
        BoardState::Failed => "pane exited without completing".into(),
        BoardState::Review => match (row.pr_number, row.branch.as_deref()) {
            (Some(n), _) => format!("PR #{n} · waiting on you"),
            // Finished on commits with no PR raised: say which branch, or the
            // row reads as "waiting on you" with nowhere to look.
            (None, Some(b)) => format!("{b} · no PR"),
            (None, None) => "waiting on you".into(),
        },
        BoardState::Ready => {
            // The route rides the group header and the repo the leading token
            // (gh#125), so the sub-line keeps only what neither says: a routed
            // workspace whose name differs from the route's, and the cursor's
            // one affordance.
            let ws = row
                .workspace
                .as_deref()
                .filter(|w| row.route.as_deref() != Some(w))
                .unwrap_or_default();
            if !row.dispatchable {
                // A property of the issue, not an affordance for the cursor —
                // so it shows on every such row, selected or not.
                if ws.is_empty() {
                    NO_ROUTE.into()
                } else {
                    format!("{ws} · {NO_ROUTE}")
                }
            } else if selected {
                if ws.is_empty() {
                    "[enter to dispatch]".into()
                } else {
                    format!("{ws} · [enter to dispatch]")
                }
            } else {
                ws.to_string()
            }
        }
        BoardState::Done => {
            // A row whose issue was deleted sits in `done` next to rows that
            // were properly closed, and the two are worth telling apart.
            if row.gone {
                format!(
                    "{}{}",
                    fixed(row.runtime.as_deref().unwrap_or(""), 12),
                    "gone upstream"
                )
            } else {
                format!(
                    "{}{}",
                    fixed(row.runtime.as_deref().unwrap_or(""), 12),
                    fixed(&ws(row), 11)
                )
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Whose subscription a run spends (gh#101)
// ---------------------------------------------------------------------------
//
// The billing guard's whole vocabulary lives here, for the same reason the
// section order does: four surfaces say it — the desktop picker, the TUI
// picker, the `comet-board` CLI and the upstream comment the board writes — and
// a warning worded three ways is three warnings nobody recognises as the same
// one. `comet-board` owns the *policy* (the `billing_guard` mode and the
// refusal); these are the words and the comparison.

/// The email whose subscription a dispatch would spend, out of the logins a
/// device has saved.
///
/// `slot` is the agent-account id the dispatch names — the route's, or the
/// picker's override. `None` is the box's own CLI login, which is the *active*
/// account for that harness: the same login a run with no slot at all would
/// reach, so naming it is describing what will happen rather than guessing.
///
/// `None` back means the device cannot name one — no such slot, no live login,
/// or a login whose credentials it could not read far enough to find an email.
/// Nothing accuses anybody on the strength of that: [`cross_billed`] is false
/// whenever this is `None`.
pub fn billed_email<'a>(
    accounts: &'a [crate::AgentAccount],
    harness: crate::HarnessId,
    slot: Option<&str>,
) -> Option<&'a str> {
    let account = match slot.filter(|s| !s.is_empty()) {
        Some(slot) => accounts
            .iter()
            .find(|a| a.harness == harness && a.id == slot)?,
        None => accounts.iter().find(|a| a.harness == harness && a.active)?,
    };
    account.email.as_deref().filter(|e| !e.is_empty())
}

/// Is this run spending somebody else's subscription?
///
/// The match is claim-vs-slot-email and nothing more: `dispatcher` is the
/// `viaUser` the dispatching frontend sent, which the box cannot verify (see
/// [`TaskRow::dispatched_by_user`]). Two unknowns therefore read as "not
/// cross-billed" rather than as an accusation — an unattributed dispatch (the
/// bare CLI, an orchestrating agent) names nobody to have wronged.
pub fn cross_billed(billed_to: Option<&str>, dispatcher: Option<&str>) -> bool {
    let (Some(billed), Some(by)) = (email(billed_to), email(dispatcher)) else {
        return false;
    };
    !billed.eq_ignore_ascii_case(by)
}

/// The phrase a `require-own` refusal carries, so a frontend can tell "the box
/// minds who pays for this" from every other reason a dispatch failed and offer
/// the confirm instead of a dead end.
///
/// A shared constant rather than each side's own substring: the refusal is
/// written by `comet-board` and read by the desktop panel, and a reworded
/// message that quietly stopped matching would turn the confirm into an error
/// nobody could act on.
pub const REQUIRE_OWN_REFUSAL: &str = "billing_guard = \"require-own\"";

/// A non-empty, trimmed email, or nothing.
fn email(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

/// What a picker row says about a selection that bills somebody else — the
/// short form, because it rides beside a chip that is already naming a login.
pub fn bills_label(billed_to: &str) -> String {
    format!("bills {}", billed_to.trim())
}

/// The one line a CLI prints, and a refusal leads with, before a cross-billed
/// release. Names the subscription (`Claude`, `Codex`) rather than the harness
/// id, because the person about to be charged thinks of it by the product's
/// name and not by comet's runtime spelling.
pub fn bills_warning(billed_to: &str, harness: crate::HarnessId) -> String {
    format!(
        "this run bills {}'s {} — pass --account <your slot>",
        billed_to.trim(),
        subscription_noun(harness)
    )
}

/// What the upstream dispatch comment appends when the run is cross-billed, so
/// the record is public to both parties rather than living on one usage page.
pub fn bills_comment_suffix(billed_to: &str) -> String {
    format!(" · on {}'s subscription", billed_to.trim())
}

/// The subscription a harness spends, named the way its owner would.
pub fn subscription_noun(harness: crate::HarnessId) -> &'static str {
    match harness {
        crate::HarnessId::ClaudeCode => "Claude",
        crate::HarnessId::Codex => "Codex",
        crate::HarnessId::Cursor => "Cursor",
        crate::HarnessId::Opencode => "OpenCode",
        crate::HarnessId::Mock => "mock",
    }
}

/// What a board row says about who it is charging, for the life of the attempt
/// — `None` when nobody is being charged for somebody else.
///
/// Derived from the row alone, which is what lets both viewports show it
/// without asking the box anything: the attempt recorded whose subscription it
/// spends, and it recorded who said they released it.
pub fn billing_note(row: &TaskRow) -> Option<String> {
    let billed = row.billed_to.as_deref()?;
    cross_billed(Some(billed), row.dispatched_by_user.as_deref()).then(|| bills_label(billed))
}

fn ws(row: &TaskRow) -> String {
    row.workspace
        .as_deref()
        .map(|w| format!("ws:{w}"))
        .unwrap_or_default()
}

/// Pad or truncate to an exact column width.
fn fixed(s: &str, width: usize) -> String {
    let t = truncate(s, width);
    let n = t.chars().count();
    format!("{t}{}", " ".repeat(width.saturating_sub(n)))
}

/// Truncate to `max` display cells, marking the cut with `…`.
pub fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Live agents — the sidebar's Agents section (gh#103)
// ---------------------------------------------------------------------------
//
// herdr gave presence away: a working agent was a pane, and the pane list was
// the sidebar. Here a dispatched agent is a chat among chats, so tracking five
// of them meant opening the board pane or nothing. This is that list, rebuilt
// from what is already streamed — board rows joined to chats, state read live
// off the session watch. Pure presentation: nothing here dispatches, settles or
// decides anything, and a row leaves the list only because the attempt it names
// stopped being live.

/// What a live attempt's agent is doing right now.
///
/// Three states where the board has two, and the split is the point. The board
/// calls a dead run and an agent asking a question both `blocked` — correctly,
/// since both hold a chat and a concurrency slot — but they ask different
/// things of a human: one wants an answer, the other a retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    /// Waiting on a human: a question, or a permission prompt.
    Blocked,
    /// Its run died. Still a live attempt — a retry is the *same* attempt.
    Errored,
    Working,
}

impl AgentState {
    /// Sidebar order — lower is more urgent, and this is why blocked floats.
    ///
    /// The same ranking [`crate::view::attention_rank`] gives chat rows, for the
    /// same reason: a question outranks a corpse, which outranks work that is
    /// going fine on its own.
    pub fn rank(self) -> u8 {
        match self {
            AgentState::Blocked => 0,
            AgentState::Errored => 1,
            AgentState::Working => 2,
        }
    }

    /// Worth interrupting a human for — what the section's count badge counts.
    pub fn needs_attention(self) -> bool {
        !matches!(self, AgentState::Working)
    }

    /// The board's own glyphs, so a row means the same thing in the sidebar as
    /// it does in the board pane one keystroke away.
    pub fn glyph(self) -> &'static str {
        match self {
            AgentState::Blocked => BoardState::Blocked.glyph(),
            AgentState::Errored => BoardState::Failed.glyph(),
            AgentState::Working => BoardState::Working.glyph(),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AgentState::Blocked => "blocked",
            AgentState::Errored => "errored",
            AgentState::Working => "working",
        }
    }
}

/// One live attempt, as the sidebar draws it.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentRow {
    pub task_id: String,
    /// Where the click goes. Always a chat that exists — see [`agent_rows`].
    pub chat_id: String,
    /// The issue identifier (`AGE-14`, `gh#103`): what the agent is *for*, and
    /// a better title than the chat's, which the agent writes about itself.
    pub identifier: String,
    pub branch: Option<String>,
    pub state: AgentState,
    /// When the attempt started. `None` on a row carrying no `started_at`,
    /// which is a board that predates the field.
    ///
    /// The instant rather than the age, so a viewport can re-read the clock on
    /// its own frames instead of rebuilding this list once a second to move a
    /// counter — the same rule the spinners follow.
    pub started_at: Option<DateTime<Utc>>,
    /// The route's `max_duration`, when it has one.
    pub cap_secs: Option<u64>,
}

impl AgentRow {
    /// How long this attempt has been going.
    pub fn elapsed_secs(&self, now: DateTime<Utc>) -> Option<i64> {
        agent_elapsed_secs(self.started_at, now)
    }

    /// Past its cap: gh#70's clock is warning it now and will cancel it next.
    pub fn over_cap(&self, now: DateTime<Utc>) -> bool {
        agent_over_cap(self.started_at, self.cap_secs, now)
    }

    /// `1h50m / 2h`, bare elapsed on an uncapped route, or nothing.
    pub fn elapsed_label(&self, now: DateTime<Utc>) -> Option<String> {
        agent_elapsed_label(self.started_at, self.cap_secs, now)
    }
}

// The same three answers as free functions, for a viewport that has flattened
// the row into its own model and holds the two fields rather than the struct.

/// Seconds since an attempt started. Never negative: clock skew must not read
/// as a count-up from the future.
pub fn agent_elapsed_secs(started_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<i64> {
    Some((now - started_at?).num_seconds().max(0))
}

/// Past the route's cap. The *decision* stays board-side
/// (`comet_board::overrun`) — this is the display's half, and it deliberately
/// says nothing about the grace, which is the board's to spend.
pub fn agent_over_cap(
    started_at: Option<DateTime<Utc>>,
    cap_secs: Option<u64>,
    now: DateTime<Utc>,
) -> bool {
    match (agent_elapsed_secs(started_at, now), cap_secs) {
        (Some(elapsed), Some(cap)) => elapsed as u64 >= cap,
        _ => false,
    }
}

/// `1h50m / 2h` — or bare elapsed where the route caps nothing, or nothing at
/// all where the row cannot say when it started.
pub fn agent_elapsed_label(
    started_at: Option<DateTime<Utc>>,
    cap_secs: Option<u64>,
    now: DateTime<Utc>,
) -> Option<String> {
    let elapsed = format_elapsed(agent_elapsed_secs(started_at, now)?);
    Some(match cap_secs {
        Some(cap) => format!("{elapsed} / {}", format_cap(cap)),
        None => elapsed,
    })
}

/// Every live attempt with a chat to open, most urgent first.
///
/// The three inputs are the three standing streams every viewport already
/// holds: `WatchBoard` rows, the chat rows, and the session watch.
///
/// - **A live attempt is `working` or `blocked` with a chat id.** That is the
///   whole membership rule, and it is why the row leaves on its own: settle,
///   cancel and orphan all end the attempt, which clears `chat_id` and moves
///   the row out of both states in the same frame. The chat stays findable
///   under its space, as it always was.
/// - **The chat must exist here.** A row whose chat has not synced (or is not
///   shared with this person) is dropped rather than drawn as something that
///   cannot be opened.
/// - **State comes from the session watch, not from the row.** The board's
///   state is a sync cycle old; the session mirror is live, and staleness-gated
///   ([`crate::view::effective_indicator`]) so a crashed backend cannot leave an
///   eternal spinner in the sidebar. The row's state is the fallback for a chat
///   with no session mirror yet — a dispatch whose first run has not started.
pub fn agent_rows(
    rows: &[TaskRow],
    chats: &[crate::Chat],
    sessions: &[crate::Session],
    now: DateTime<Utc>,
) -> Vec<AgentRow> {
    let mut out: Vec<AgentRow> = rows
        .iter()
        .filter(|row| row.state().holds_pane())
        .filter_map(|row| {
            let chat_id = row.chat_id.as_deref()?;
            let chat = chats.iter().find(|c| c.id == chat_id)?;
            let session = sessions.iter().find(|s| s.chat_id == chat_id);
            Some(AgentRow {
                task_id: row.id.clone(),
                chat_id: chat_id.to_string(),
                identifier: row.identifier.clone(),
                // The chat's branch first: it is the checkout the agent is
                // actually in, and the attempt row's copy is what it was cut as.
                branch: chat
                    .branch
                    .as_deref()
                    .or(row.branch.as_deref())
                    .map(str::trim)
                    .filter(|b| !b.is_empty())
                    .map(str::to_string),
                state: agent_state(row.state(), session, now),
                started_at: row
                    .started_at
                    .as_deref()
                    .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
                    .map(|at| at.with_timezone(&Utc)),
                cap_secs: row.max_duration_secs,
            })
        })
        .collect();
    // Urgency first, then longest-running — which is stable, since that order is
    // start order and start order never changes under a viewer. A row that
    // cannot say when it started sorts last; the identifier breaks the final tie
    // so the sort is total.
    out.sort_by(|a, b| {
        a.state
            .rank()
            .cmp(&b.state.rank())
            .then_with(|| match (a.started_at, b.started_at) {
                (Some(a), Some(b)) => a.cmp(&b),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
            .then_with(|| a.identifier.cmp(&b.identifier))
    });
    out
}

/// What the session mirror says about this attempt, falling back to the board.
/// `pub(crate)` for the Needs-you inbox ([`crate::view::needs`]), which must
/// reach the same verdict about the same attempt.
pub(crate) fn agent_state(
    state: BoardState,
    session: Option<&crate::Session>,
    now: DateTime<Utc>,
) -> AgentState {
    match crate::view::effective_indicator(session, now) {
        crate::view::Indicator::Working => AgentState::Working,
        crate::view::Indicator::AwaitingInput => AgentState::Blocked,
        crate::view::Indicator::Errored => AgentState::Errored,
        // No live session: idle, stale, or never started. The board's verdict
        // is older but it is a verdict, and `blocked` is the one it reaches for
        // a run that ended without settling.
        crate::view::Indicator::None => match state {
            BoardState::Blocked => AgentState::Blocked,
            _ => AgentState::Working,
        },
    }
}

/// How many live agents want a human — the section header's count badge.
pub fn agents_needing_attention(rows: &[AgentRow]) -> usize {
    rows.iter().filter(|row| row.state.needs_attention()).count()
}

// ---------------------------------------------------------------------------
// Running — every working chat the board is NOT running (gh#117)
// ---------------------------------------------------------------------------
//
// [`agent_rows`] answers "what has the board released", which is a smaller
// question than "what is working on this box". An orchestrator that raised
// in-chat subagents instead of dispatching, an ad-hoc chat somebody started by
// hand: all of them are real runs holding a real checkout, and none of them
// has an attempt row, so the Agents section shows nothing and the only honest
// answer to "are they even alive" was `pgrep` over ssh. This is the other half
// of the list. (The pinned orchestrator, which used to lead this group, now
// has a fixed slot of its own — gh#122 — and is subtracted here.)
//
// The join is the same one gh#103 does, minus the board row — which is exactly
// why it is cheap: the session watch already streams a status for every chat,
// and a box hosting no board at all still fills this group.

/// What a chat with no title is called — the spelling all three sessions lists
/// already use, pinned here so the Running group cannot drift from them.
///
/// The two lists are a few rows apart, and one untitled chat reading
/// `New session` in one and something else in the other looks like two chats.
pub const UNTITLED_CHAT: &str = "New session";

/// One working chat that no board attempt accounts for.
///
/// Deliberately thinner than [`AgentRow`]: there is no issue, no branch
/// promised, no cap and no attempt behind it, so the row says only what is
/// knowable — what the chat calls itself, how long its run has been going, and
/// whether it is waiting on a person.
#[derive(Debug, Clone, PartialEq)]
pub struct RunningRow {
    /// Where the click goes, and this row's identity.
    pub chat_id: String,
    /// The chat's own title ([`UNTITLED_CHAT`] when it has none). The agent
    /// wrote it about itself, which is second-best to an issue identifier and
    /// the best thing there is when no issue exists.
    pub title: String,
    /// The space to land in when the row is opened, as the chat records it.
    pub space_id: Option<String>,
    /// `Working` or `Blocked` and never `Errored` — membership is the live
    /// indicator, and an errored run is not a working one.
    pub state: AgentState,
    /// When the *run* started, off the session mirror — not when the chat was
    /// created, which for a long-lived orchestrator is days ago and says
    /// nothing about the work in front of it. `None` where the mirror carries
    /// no start (an engine that predates the field).
    ///
    /// The instant rather than the age, on the same rule [`AgentRow`] follows:
    /// a viewport re-reads the clock on its own frames instead of rebuilding
    /// this list once a second to move a counter.
    pub started_at: Option<DateTime<Utc>>,
}

impl RunningRow {
    pub fn elapsed_secs(&self, now: DateTime<Utc>) -> Option<i64> {
        agent_elapsed_secs(self.started_at, now)
    }

    /// Bare elapsed — nothing caps these runs, so there is no second number to
    /// read it against. `None` where the mirror cannot say when it started.
    pub fn elapsed_label(&self, now: DateTime<Utc>) -> Option<String> {
        agent_elapsed_label(self.started_at, None, now)
    }
}

/// Every working chat that is not a live board attempt, most urgent first.
///
/// - **Membership is the session watch and nothing else.** `Working` or
///   `AwaitingInput`, staleness-gated by [`crate::view::effective_indicator`],
///   so the group fills within one watch frame of a run starting and empties
///   within one of it stopping. No board row is consulted to get *in*.
/// - **A live attempt is subtracted, not re-drawn.** Any chat claimed by a
///   `working`/`blocked` row belongs to the Agents group, which knows its
///   issue, branch and cap; showing it twice would double-count the box's
///   load. The subtraction reads the board rows directly rather than
///   [`agent_rows`]'s output, so a claimed chat stays out of this group even
///   in the case that drops it from the other one.
/// - **Archived is not a reason to hide a run.** Archiving is a decision about
///   a *finished* chat; one that is working anyway is the exact invisible run
///   this group exists to surface.
/// - **The pinned orchestrator is subtracted too** (gh#122): it has a fixed
///   slot of its own above Spaces, which carries its live state — a second
///   row here would report the same run twice.
///
/// A board is not required. `rows` empty — no board on this box, the host sweep
/// still running, a phone that has not attached — subtracts nothing, and the
/// group is the whole live list.
pub fn running_rows(
    rows: &[TaskRow],
    chats: &[crate::Chat],
    sessions: &[crate::Session],
    orchestrator: Option<&str>,
    now: DateTime<Utc>,
) -> Vec<RunningRow> {
    let dispatched: Vec<&str> = rows
        .iter()
        .filter(|row| row.state().holds_pane())
        .filter_map(|row| row.chat_id.as_deref())
        .collect();
    let mut out: Vec<RunningRow> = chats
        .iter()
        .filter(|chat| !dispatched.contains(&chat.id.as_str()))
        .filter(|chat| orchestrator != Some(chat.id.as_str()))
        .filter_map(|chat| {
            let session = sessions.iter().find(|s| s.chat_id == chat.id);
            let state = match crate::view::effective_indicator(session, now) {
                crate::view::Indicator::Working => AgentState::Working,
                crate::view::Indicator::AwaitingInput => AgentState::Blocked,
                // Errored and None are not runs. A dead chat is the sessions
                // list's to report, at the recency it earned.
                _ => return None,
            };
            Some(RunningRow {
                chat_id: chat.id.clone(),
                title: chat
                    .title
                    .as_deref()
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .unwrap_or(UNTITLED_CHAT)
                    .to_string(),
                space_id: chat.space_id.clone(),
                state,
                started_at: session.and_then(|s| s.started_at),
            })
        })
        .collect();
    // The same order the Agents group uses, for the same reason: a question
    // outranks work going fine, and under that, longest-running first is stable
    // because start order never changes under a viewer. The chat id breaks the
    // final tie so the sort is total — titles are not unique and change under
    // the reader as an agent renames its own chat.
    out.sort_by(|a, b| {
        a.state
            .rank()
            .cmp(&b.state.rank())
            .then_with(|| match (a.started_at, b.started_at) {
                (Some(a), Some(b)) => a.cmp(&b),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
            .then_with(|| a.chat_id.cmp(&b.chat_id))
    });
    out
}

/// How many of these want a human — the Running header's count badge.
pub fn running_needing_attention(rows: &[RunningRow]) -> usize {
    rows.iter().filter(|row| row.state.needs_attention()).count()
}

// ---------------------------------------------------------------------------
// Which device hosts the board (gh#55)
// ---------------------------------------------------------------------------

/// Evidence that a board has ever been dispatched from: any row with an attempt
/// on record (gh#125).
///
/// This is what an automatic host sweep settles on. A board *service* answers
/// `WatchBoard` wherever `COMET_BOARD` is unset, so "delivered a frame" proves
/// only that a board exists — a laptop's stale test board answers as readily as
/// the box the org actually works from, and it answers *first* because the
/// sweep asks this device before the others. Dispatch is the difference: a
/// board somebody has released work from is the org's board; one that only ever
/// collected rows is furniture. The sweep therefore holds a frame with no
/// dispatch evidence as a *fallback* and keeps asking, settling on it only when
/// no candidate with evidence answers — so a lone device, and a genuinely
/// fresh install, still see their own board.
///
/// `attempts`, not `chat_id`: the chat id rides only the live attempt, and the
/// box's board between dispatches must not read as furniture.
pub fn board_dispatched(rows: &[TaskRow]) -> bool {
    rows.iter().any(|row| row.attempts > 0)
}

/// The devices a board pane tries, in order, when the operator has pinned none.
/// `None` means this device — no `targetDeviceId` passthrough.
///
/// The board store lives on exactly ONE device (docs/BOARD.md: one host device
/// is correct while one box hosts the board), and the board RPCs are
/// relay-forwardable, so a viewport that finds no board locally is not out of
/// options — it just has to ask the other devices. This device comes first
/// because asking locally is free; whether its answer *settles* the sweep is
/// [`board_dispatched`]'s question. The rest follow in registration order,
/// which is stable across heartbeats (the same order the device switcher lists
/// them in), so the sweep visits them the same way twice.
///
/// A candidate is ruled out by its `WatchBoard` stream ending without ever
/// delivering a frame — the engine refuses the subscription outright when it
/// hosts no board, so "said nothing at all" IS the answer.
pub fn host_candidates(devices: &[crate::Device], local_device_id: Option<&str>) -> Vec<Option<String>> {
    let mut others: Vec<&crate::Device> = devices
        .iter()
        .filter(|d| Some(d.id.as_str()) != local_device_id)
        .collect();
    others.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
    std::iter::once(None)
        .chain(others.into_iter().map(|d| Some(d.id.clone())))
        .collect()
}

/// The candidate after `current` in the sweep, or `None` when the sweep is
/// exhausted (every device has been asked and none hosts a board).
///
/// Returns `Some(next)` where `next` is itself the target — `Some(None)` is
/// "try this device", `Some(Some(id))` is "try that device". A `current` that
/// has left the list (a device deregistered mid-sweep) restarts the sweep at
/// the top rather than ending it on a stale position.
pub fn next_host_candidate(
    candidates: &[Option<String>],
    current: Option<&str>,
) -> Option<Option<String>> {
    match candidates.iter().position(|c| c.as_deref() == current) {
        Some(here) => candidates.get(here + 1).cloned(),
        None => candidates.first().cloned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::board::{Filter, TaskRow};

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-08-01T12:00:00Z").unwrap().with_timezone(&Utc)
    }

    fn row(id: &str, state: BoardState) -> TaskRow {
        TaskRow {
            id: id.into(),
            identifier: format!("gh#{id}"),
            title: format!("task {id}"),
            state: state.as_str().into(),
            source: "github".into(),
            url: format!("https://github.com/o/r/issues/{id}"),
            labels: vec![],
            dispatchable: true,
            gone: false,
            route: Some("offhand".into()),
            workspace: Some("offhand".into()),
            runtime: Some("claude-code".into()),
            chat_id: None,
            pr_url: None,
            pr_number: None,
            branch: Some("board/gh-x".into()),
            dispatched_by: None,
            dispatched_by_chat: None,
            last_outcome: None,
            last_outcome_at: None,
            attempts: 0,
            reopened: 0,
            updated_at: "2026-08-01T11:00:00Z".into(),
            started_at: None,
            account: None,
            dispatched_by_user: None,
            billed_to: None,
            max_duration_secs: None,
        }
    }

    #[test]
    fn sections_are_in_fixed_order_and_omit_empties() {
        let rows = vec![
            row("1", BoardState::Ready),
            row("2", BoardState::Working),
            row("3", BoardState::Done),
            row("4", BoardState::Blocked),
        ];
        let order: Vec<BoardState> = sections(&rows, &Filter::All, now())
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        assert_eq!(
            order,
            vec![
                BoardState::Blocked,
                BoardState::Working,
                BoardState::Ready,
                BoardState::Done
            ]
        );
        // review and failed had no rows and are absent entirely.
        assert!(!order.contains(&BoardState::Review));
    }

    #[test]
    fn done_is_bounded_to_today() {
        let mut old = row("old", BoardState::Done);
        old.updated_at = "2020-01-01T00:00:00Z".into();
        let mut today = row("today", BoardState::Done);
        today.updated_at = "2026-08-01T09:00:00Z".into();
        let rows = vec![old, today, row("r", BoardState::Ready)];
        let done: Vec<&str> = sections(&rows, &Filter::All, now())
            .into_iter()
            .find(|(s, _)| *s == BoardState::Done)
            .map(|(_, rows)| rows.into_iter().map(|r| r.id.as_str()).collect())
            .unwrap_or_default();
        assert_eq!(done, vec!["today"]);
    }

    #[test]
    fn a_done_section_with_nothing_from_today_is_omitted() {
        let mut old = row("old", BoardState::Done);
        old.updated_at = "2020-01-01T00:00:00Z".into();
        let rows = vec![old, row("r", BoardState::Ready)];
        assert!(
            !sections(&rows, &Filter::All, now())
                .iter()
                .any(|(s, _)| *s == BoardState::Done),
            "an all-history done section must not render at all"
        );
    }

    #[test]
    fn f_cycles_the_routes_on_the_board_and_then_back_to_all() {
        let mut rows = vec![
            row("a", BoardState::Ready),
            row("b", BoardState::Working),
            row("c", BoardState::Ready),
            row("d", BoardState::Blocked),
        ];
        rows[1].route = Some("itsm-agent".into());
        rows[2].route = Some("itsm-agent".into());
        rows[3].route = Some("tally".into());
        assert_eq!(routes_present(&rows, now()), vec!["itsm-agent", "offhand", "tally"]);
        assert_eq!(
            filter_cycle(&rows, now()),
            vec![
                Filter::Route("itsm-agent".into()),
                Filter::Route("offhand".into()),
                Filter::Route("tally".into()),
            ]
        );
    }

    #[test]
    fn no_route_is_a_position_in_the_cycle_after_the_named_routes() {
        let mut rows = vec![row("a", BoardState::Ready), row("u", BoardState::Ready)];
        rows[1].route = None;
        let cycle = filter_cycle(&rows, now());
        assert_eq!(
            cycle,
            vec![Filter::Route("offhand".into()), Filter::NoRoute]
        );
        assert!(
            Filter::NoRoute.matches(&rows[1]),
            "the unrouted row is the position's contents"
        );
        assert!(!Filter::NoRoute.matches(&rows[0]));
    }

    #[test]
    fn no_route_is_skipped_when_everything_routes() {
        let rows = vec![row("a", BoardState::Ready)];
        assert_eq!(filter_cycle(&rows, now()), vec![Filter::Route("offhand".into())]);
    }

    #[test]
    fn the_filter_is_a_view_and_a_hidden_row_stays_hidden() {
        let mut rows = vec![row("a", BoardState::Ready), row("b", BoardState::Working)];
        rows[1].route = Some("itsm-agent".into());
        let shown: Vec<&str> = sections(&rows, &Filter::Route("itsm-agent".into()), now())
            .into_iter()
            .flat_map(|(_, rows)| rows)
            .map(|r| r.id.as_str())
            .collect();
        assert_eq!(shown, vec!["b"]);
        // Nothing about the rows themselves changed — the filter is a view.
        assert_eq!(rows[1].route.as_deref(), Some("itsm-agent"));
    }

    #[test]
    fn a_typed_query_matches_identifier_title_route_and_no_route() {
        let mut rows = [row("a", BoardState::Ready), row("b", BoardState::Ready)];
        rows[1].route = None;
        assert!(Filter::Text("gh#a".into()).matches(&rows[0]));
        assert!(Filter::Text("task b".into()).matches(&rows[1]));
        assert!(Filter::Text("offhand".into()).matches(&rows[0]));
        assert!(Filter::Text("no route".into()).matches(&rows[1]), "`/no route` must reach the group");
        assert!(!Filter::Text("zzz".into()).matches(&rows[0]));
        // An empty query is the whole board, not nothing.
        assert!(Filter::Text("".into()).matches(&rows[0]));
    }

    #[test]
    fn filters_say_what_they_are_and_what_to_do_when_empty() {
        assert_eq!(Filter::All.label(), None);
        assert_eq!(Filter::Route("x".into()).label(), Some("filter: x".into()));
        assert_eq!(Filter::NoRoute.label(), Some("filter: no route".into()));
        assert_eq!(Filter::Text("hi".into()).label(), Some("/hi".into()));
        assert_eq!(
            Filter::Route("x".into()).empty_note(),
            Some("Nothing on the board routes to x.  f moves on, F clears the filter.".into())
        );
        assert_eq!(Filter::All.empty_note(), None);
        assert_eq!(Filter::Text("".into()).empty_note(), None);
    }

    #[test]
    fn ready_metadata_keeps_only_what_the_grouping_does_not_already_say() {
        // The route is the group header and the repo the leading token
        // (gh#125): a workspace matching the route's name adds nothing.
        let rows = [row("a", BoardState::Ready)];
        assert_eq!(row_metadata(&rows[0], false, 80, now()), "");
        // Selected: the one action the cursor can take.
        assert!(row_metadata(&rows[0], true, 80, now()).contains("[enter to dispatch]"));

        // A workspace the route's name does not say survives on the sub-line.
        let mut renamed = row("b", BoardState::Ready);
        renamed.route = Some("tally".into());
        assert_eq!(row_metadata(&renamed, false, 80, now()), "offhand");

        // Nothing routes: the same words the filter uses.
        let mut unrouted = row("u", BoardState::Ready);
        unrouted.route = None;
        unrouted.workspace = None;
        unrouted.dispatchable = false;
        assert_eq!(row_metadata(&unrouted, false, 80, now()), "no route");
    }

    #[test]
    fn the_leading_token_is_the_repo_qualified_form_humanized() {
        let mut r = row("507", BoardState::Ready);
        r.id = "gh:Florin-AS/tally#507".into();
        r.identifier = "gh#507".into();
        assert_eq!(r.display_identifier(), "tally #507");
        // The pull-request form of the id parses the same way.
        r.id = "gh:Florin-AS/tally!508".into();
        assert_eq!(r.display_identifier(), "tally #508");
        // A Linear identifier is already unique across the board.
        r.id = "linear:LIN-142".into();
        r.identifier = "LIN-142".into();
        assert_eq!(r.display_identifier(), "LIN-142");
        // An id this rule cannot parse degrades to the identifier, never to
        // half a lockup.
        r.id = "gh:mangled".into();
        r.identifier = "gh#9".into();
        assert_eq!(r.display_identifier(), "gh#9");
        // What is rendered is what `/` matches.
        r.id = "gh:Florin-AS/tally#507".into();
        assert!(Filter::Text("tally #5".into()).matches(&r));
    }

    #[test]
    fn groups_rank_by_size_with_no_route_last_and_folded() {
        let mut rows = vec![
            row("t1", BoardState::Ready),
            row("t2", BoardState::Ready),
            row("u", BoardState::Ready),
            row("h1", BoardState::Ready),
        ];
        rows[0].route = Some("tally".into());
        rows[1].route = Some("tally".into());
        rows[2].route = None;
        rows[2].dispatchable = false;
        rows[3].route = Some("herdr-board".into());
        let grouped = grouped_sections(&rows, &Filter::All, now());
        assert_eq!(grouped.len(), 1);
        let (state, groups) = &grouped[0];
        assert_eq!(*state, BoardState::Ready);
        let labels: Vec<&str> = groups.iter().map(|g| g.label()).collect();
        // Biggest first, `no route` last regardless of size — never pole
        // position.
        assert_eq!(labels, vec!["tally", "herdr-board", "no route"]);
        assert!(
            group_starts_collapsed(&Filter::All, None),
            "unrouted rows start folded"
        );
        assert!(!group_starts_collapsed(&Filter::All, Some("tally")));
        assert!(group_headers_shown(&Filter::All, groups));
    }

    #[test]
    fn a_filter_unfolds_what_it_asked_for() {
        // `f` to the `no route` position: the rows must show, not sit behind
        // the very fold the filter was meant to reach past.
        assert!(!group_starts_collapsed(&Filter::NoRoute, None));
        // `/` matching an unrouted title: a search that hides its own match
        // reads as no match at all.
        assert!(!group_starts_collapsed(&Filter::Text("signicat".into()), None));
        // And the lone group under that filter needs no header repeating what
        // the filter chip already says.
        let mut unrouted = vec![row("u", BoardState::Ready)];
        unrouted[0].route = None;
        let grouped = grouped_sections(&unrouted, &Filter::NoRoute, now());
        assert!(!group_headers_shown(&Filter::NoRoute, &grouped[0].1));
    }

    #[test]
    fn equal_sized_groups_hold_alphabetical_order_between_frames() {
        let mut rows = vec![row("b1", BoardState::Ready), row("a1", BoardState::Ready)];
        rows[0].route = Some("zeta".into());
        rows[1].route = Some("alpha".into());
        let grouped = grouped_sections(&rows, &Filter::All, now());
        let labels: Vec<&str> = grouped[0].1.iter().map(|g| g.label()).collect();
        assert_eq!(labels, vec!["alpha", "zeta"]);
    }

    #[test]
    fn a_single_routed_group_draws_no_header_but_a_lone_no_route_group_does() {
        let rows = vec![row("a", BoardState::Working)];
        let grouped = grouped_sections(&rows, &Filter::All, now());
        assert!(
            !group_headers_shown(&Filter::All, &grouped[0].1),
            "one routed group is readable bare"
        );

        let mut unrouted = vec![row("u", BoardState::Ready)];
        unrouted[0].route = None;
        let grouped = grouped_sections(&unrouted, &Filter::All, now());
        assert!(
            group_headers_shown(&Filter::All, &grouped[0].1),
            "the header is what keeps unrouted rows folded"
        );
    }

    #[test]
    fn dispatch_evidence_is_any_attempt_on_record_not_a_live_chat() {
        let mut rows = vec![row("a", BoardState::Ready), row("b", BoardState::Done)];
        assert!(!board_dispatched(&rows), "a board that only collected rows is furniture");
        // The box between dispatches: nothing live, history on record.
        rows[1].attempts = 3;
        assert!(board_dispatched(&rows));
        assert!(!board_dispatched(&[]), "an empty board proves nothing either way");
    }

    #[test]
    fn review_metadata_names_the_pr_or_the_branch() {
        let mut r = row("r", BoardState::Review);
        r.pr_number = Some(7);
        assert_eq!(row_metadata(&r, false, 80, now()), "PR #7 · waiting on you");
        r.pr_number = None;
        assert!(row_metadata(&r, false, 80, now()).contains("· no PR"));
        r.branch = None;
        assert_eq!(row_metadata(&r, false, 80, now()), "waiting on you");
    }

    #[test]
    fn done_metadata_says_gone_upstream_or_the_workspace() {
        let mut r = row("d", BoardState::Done);
        assert!(row_metadata(&r, false, 80, now()).contains("ws:offhand"));
        r.gone = true;
        assert!(row_metadata(&r, false, 80, now()).contains("gone upstream"));
    }

    #[test]
    fn working_metadata_names_the_runtime_workspace_and_elapsed() {
        let mut r = row("w", BoardState::Working);
        r.started_at = Some("2026-08-01T11:59:30Z".into());
        let wide = row_metadata(&r, false, 120, now());
        assert!(wide.contains("claude-code"), "runtime: {wide}");
        assert!(wide.contains("ws:offhand"), "workspace: {wide}");
        assert!(wide.contains("30s"), "elapsed: {wide}");
        // Below the narrow limit everything goes rather than wrapping.
        assert_eq!(row_metadata(&r, false, 59, now()), "");
    }

    #[test]
    fn elapsed_never_goes_negative() {
        let mut r = row("w", BoardState::Working);
        r.started_at = Some("2026-08-01T12:01:00Z".into());
        assert!(row_metadata(&r, false, 80, now()).contains("0s"));
    }

    #[test]
    fn unknown_wire_state_reads_as_ready_not_a_crash() {
        let mut r = row("x", BoardState::Ready);
        r.state = "bogus".into();
        assert_eq!(r.state(), BoardState::Ready);
    }

    #[test]
    fn task_row_round_trips_through_json() {
        let r = row("a", BoardState::Review);
        let json = serde_json::to_string(&r).unwrap();
        let back: TaskRow = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
        // A pre-timestamp row still deserializes.
        let mut bare = serde_json::to_value(&r).unwrap();
        bare.as_object_mut().unwrap().remove("updated_at");
        bare.as_object_mut().unwrap().remove("started_at");
        let back: TaskRow = serde_json::from_value(bare).unwrap();
        assert_eq!(back.updated_at, "");
        assert_eq!(back.started_at, None);
    }

    #[test]
    fn format_helpers_read_as_people_mean_them() {
        assert_eq!(format_elapsed(12), "12s");
        assert_eq!(format_elapsed(9 * 60 + 4), "9m04s");
        assert_eq!(format_elapsed(3600 + 1200), "1h20m");
        assert_eq!(format_age(12), "12s");
        assert_eq!(format_age(240), "4m");
        assert_eq!(format_age(3 * 3600), "3h");
        // A cap is a round number somebody typed, and reads as one.
        assert_eq!(format_cap(7200), "2h");
        assert_eq!(format_cap(45 * 60), "45m");
        assert_eq!(format_cap(90 * 60), "1h30m");
        assert_eq!(format_cap(30), "30s");
    }

    // ---- live agents (gh#103) ---------------------------------------------

    fn chat(id: &str, branch: Option<&str>) -> crate::Chat {
        crate::Chat {
            id: id.into(),
            device_id: "box".into(),
            title: Some("some title the agent wrote".into()),
            archived: false,
            cwd: Some(format!("/w/{id}")),
            branch: branch.map(str::to_string),
            checkout_id: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: now(),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: Some("space".into()),
            last_seen_at: None,
        }
    }

    fn session(chat_id: &str, status: crate::SessionStatus, age_ms: i64) -> crate::Session {
        crate::Session {
            chat_id: chat_id.into(),
            device_id: "box".into(),
            status,
            started_at: None,
            updated_at: now() - chrono::Duration::milliseconds(age_ms),
        }
    }

    /// A live attempt is a `working`/`blocked` row with a chat, and the chat has
    /// to be one this viewport can actually open.
    #[test]
    fn only_live_attempts_with_a_reachable_chat_are_agents() {
        let mut live = row("live", BoardState::Working);
        live.chat_id = Some("chat-live".into());
        // Dispatched, but its chat has not synced here (or was never shared).
        let mut orphan = row("orphan", BoardState::Working);
        orphan.chat_id = Some("chat-elsewhere".into());
        // Finished: the attempt closed, so the row kept neither state nor chat.
        let settled = row("settled", BoardState::Review);
        let ready = row("ready", BoardState::Ready);

        let rows = vec![live, orphan, settled, ready];
        let agents = agent_rows(&rows, &[chat("chat-live", None)], &[], now());
        assert_eq!(
            agents.iter().map(|a| a.task_id.as_str()).collect::<Vec<_>>(),
            vec!["live"]
        );
    }

    /// The state on screen is the session watch's, not the board's — the board's
    /// is a sync cycle old, and the split of its one `blocked` into a question
    /// and a corpse is the whole reason the section exists.
    #[test]
    fn state_comes_from_the_session_watch_and_falls_back_to_the_row() {
        let mut asking = row("ask", BoardState::Working);
        asking.chat_id = Some("c1".into());
        let mut died = row("died", BoardState::Working);
        died.chat_id = Some("c2".into());
        let mut no_session = row("fresh", BoardState::Blocked);
        no_session.chat_id = Some("c3".into());
        let mut stale = row("stale", BoardState::Working);
        stale.chat_id = Some("c4".into());

        let chats = vec![chat("c1", None), chat("c2", None), chat("c3", None), chat("c4", None)];
        let sessions = vec![
            session("c1", crate::SessionStatus::AwaitingInput, 0),
            session("c2", crate::SessionStatus::Errored, 0),
            // Older than the staleness window: a crashed backend must not leave
            // an eternal spinner in the sidebar.
            session("c4", crate::SessionStatus::Working, crate::view::SESSION_STALE_MS + 1_000),
        ];
        let agents = agent_rows(&[asking, died, no_session, stale], &chats, &sessions, now());
        let state = |id: &str| {
            agents
                .iter()
                .find(|a| a.task_id == id)
                .map(|a| a.state)
                .unwrap()
        };
        assert_eq!(state("ask"), AgentState::Blocked);
        assert_eq!(state("died"), AgentState::Errored);
        // No session row yet: the board's verdict stands in.
        assert_eq!(state("fresh"), AgentState::Blocked);
        assert_eq!(state("stale"), AgentState::Working);
    }

    /// Blocked floats, and the badge counts everything that wants a human.
    #[test]
    fn blocked_floats_to_the_top_and_the_badge_counts_them() {
        let mut fast = row("fast", BoardState::Working);
        fast.chat_id = Some("c1".into());
        fast.started_at = Some("2026-08-01T11:59:00Z".into()); // 1m
        let mut slow = row("slow", BoardState::Working);
        slow.chat_id = Some("c2".into());
        slow.started_at = Some("2026-08-01T10:00:00Z".into()); // 2h
        let mut asking = row("ask", BoardState::Blocked);
        asking.chat_id = Some("c3".into());
        asking.started_at = Some("2026-08-01T11:55:00Z".into()); // 5m
        let mut died = row("died", BoardState::Working);
        died.chat_id = Some("c4".into());

        let chats = vec![chat("c1", None), chat("c2", None), chat("c3", None), chat("c4", None)];
        let sessions = vec![
            session("c1", crate::SessionStatus::Working, 0),
            session("c2", crate::SessionStatus::Working, 0),
            session("c3", crate::SessionStatus::AwaitingInput, 0),
            session("c4", crate::SessionStatus::Errored, 0),
        ];
        let agents = agent_rows(&[fast, slow, asking, died], &chats, &sessions, now());
        assert_eq!(
            agents.iter().map(|a| a.task_id.as_str()).collect::<Vec<_>>(),
            // A question, then a dead run, then the longest-running worker.
            vec!["ask", "died", "slow", "fast"]
        );
        assert_eq!(agents_needing_attention(&agents), 2);
    }

    /// "That one's been at it 1h50 of 2h" — a glance, which needs both numbers.
    #[test]
    fn elapsed_reads_against_the_routes_cap() {
        let mut r = row("x", BoardState::Working);
        r.chat_id = Some("c1".into());
        r.started_at = Some("2026-08-01T10:10:00Z".into()); // 1h50m
        r.max_duration_secs = Some(7200);
        let agents = agent_rows(&[r.clone()], &[chat("c1", None)], &[], now());
        assert_eq!(agents[0].elapsed_label(now()).as_deref(), Some("1h50m / 2h"));
        assert!(!agents[0].over_cap(now()));

        // Past the cap: gh#70's clock is about to end it, and the row says so.
        let mut over = r.clone();
        over.started_at = Some("2026-08-01T09:00:00Z".into()); // 3h
        let agents = agent_rows(&[over], &[chat("c1", None)], &[], now());
        assert_eq!(agents[0].elapsed_label(now()).as_deref(), Some("3h00m / 2h"));
        assert!(agents[0].over_cap(now()));

        // An uncapped route says only how long it has been.
        let mut uncapped = r.clone();
        uncapped.max_duration_secs = None;
        let agents = agent_rows(&[uncapped], &[chat("c1", None)], &[], now());
        assert_eq!(agents[0].elapsed_label(now()).as_deref(), Some("1h50m"));
        assert!(!agents[0].over_cap(now()));

        // A board with no `started_at` (predates the field) says nothing rather
        // than counting up from the epoch.
        let mut undated = r;
        undated.started_at = None;
        let agents = agent_rows(&[undated], &[chat("c1", None)], &[], now());
        assert_eq!(agents[0].elapsed_label(now()), None);
        assert!(!agents[0].over_cap(now()));
    }

    /// The sub-line is the checkout the agent is in — the chat's own branch
    /// first, the attempt's as recorded when the chat has none.
    #[test]
    fn the_sub_line_prefers_the_chats_branch() {
        let mut r = row("x", BoardState::Working);
        r.chat_id = Some("c1".into());
        r.branch = Some("board/gh-103".into());
        let renamed = [chat("c1", Some("board/gh-103-renamed"))];
        let agents = agent_rows(&[r.clone()], &renamed, &[], now());
        assert_eq!(agents[0].branch.as_deref(), Some("board/gh-103-renamed"));
        let agents = agent_rows(&[r], &[chat("c1", None)], &[], now());
        assert_eq!(agents[0].branch.as_deref(), Some("board/gh-103"));
    }

    // ---- running, non-board chats (gh#117) --------------------------------

    fn started(chat_id: &str, status: crate::SessionStatus, started: &str) -> crate::Session {
        crate::Session {
            started_at: Some(
                DateTime::parse_from_rfc3339(started)
                    .unwrap()
                    .with_timezone(&Utc),
            ),
            ..session(chat_id, status, 0)
        }
    }

    /// The whole point: a chat working with no attempt behind it is presence
    /// the board cannot report, and it shows anyway.
    #[test]
    fn a_working_chat_with_no_attempt_is_a_running_row() {
        let chats = vec![chat("orchestrator", None), chat("adhoc", None), chat("idle", None)];
        let sessions = vec![
            started("orchestrator", crate::SessionStatus::Working, "2026-08-01T11:30:00Z"),
            started("adhoc", crate::SessionStatus::AwaitingInput, "2026-08-01T11:58:00Z"),
        ];
        let running = running_rows(&[], &chats, &sessions, None, now());
        assert_eq!(
            running.iter().map(|r| r.chat_id.as_str()).collect::<Vec<_>>(),
            // Blocked floats; the idle chat is not a run at all.
            vec!["adhoc", "orchestrator"]
        );
        assert_eq!(running[0].state, AgentState::Blocked);
        assert_eq!(running[1].elapsed_label(now()).as_deref(), Some("30m00s"));
    }

    /// Every membership edge in one place: stale is dead, errored is not a run,
    /// and archiving a chat that is working anyway does not hide it.
    #[test]
    fn membership_is_the_live_session_watch_and_nothing_else() {
        let mut archived = chat("archived", None);
        archived.archived = true;
        let chats = vec![
            chat("stale", None),
            chat("errored", None),
            chat("never", None),
            archived,
        ];
        let sessions = vec![
            // A crashed backend must not leave an eternal row here either.
            session("stale", crate::SessionStatus::Working, crate::view::SESSION_STALE_MS + 1_000),
            session("errored", crate::SessionStatus::Errored, 0),
            session("archived", crate::SessionStatus::Working, 0),
        ];
        let running = running_rows(&[], &chats, &sessions, None, now());
        assert_eq!(
            running.iter().map(|r| r.chat_id.as_str()).collect::<Vec<_>>(),
            vec!["archived"],
            "archiving is a decision about a finished chat, not about a live run"
        );
    }

    /// The two groups partition the box's load: a dispatched chat is the Agents
    /// group's, and counting it twice would misreport what is running.
    #[test]
    fn a_live_attempts_chat_belongs_to_agents_and_not_to_running() {
        let mut live = row("live", BoardState::Working);
        live.chat_id = Some("c-live".into());
        // A settled attempt released its chat — if the operator kept talking in
        // it, that IS an unmanaged run and belongs here.
        let mut settled = row("settled", BoardState::Review);
        settled.chat_id = Some("c-settled".into());

        let chats = vec![chat("c-live", None), chat("c-settled", None)];
        let sessions = vec![
            session("c-live", crate::SessionStatus::Working, 0),
            session("c-settled", crate::SessionStatus::Working, 0),
        ];
        let rows = vec![live, settled];
        assert_eq!(
            running_rows(&rows, &chats, &sessions, None, now())
                .iter()
                .map(|r| r.chat_id.as_str())
                .collect::<Vec<_>>(),
            vec!["c-settled"]
        );
        // And it is in exactly one of the two groups, not neither.
        assert_eq!(
            agent_rows(&rows, &chats, &sessions, now())
                .iter()
                .map(|a| a.chat_id.as_str())
                .collect::<Vec<_>>(),
            vec!["c-live"]
        );
    }

    /// A chat claimed by an attempt stays out even when the Agents group drops
    /// it — the subtraction reads the board rows, not the other list.
    #[test]
    fn a_dispatched_chat_the_agents_group_dropped_is_still_not_running() {
        let mut live = row("live", BoardState::Blocked);
        live.chat_id = Some("c1".into());
        let chats = vec![chat("c1", None)];
        let sessions = vec![session("c1", crate::SessionStatus::Working, 0)];
        // agent_rows keeps it (the chat synced) — but the subtraction must not
        // depend on that having happened.
        assert!(!agent_rows(std::slice::from_ref(&live), &chats, &sessions, now()).is_empty());
        assert!(running_rows(&[live], &chats, &sessions, None, now()).is_empty());
    }

    /// What the row can say: the chat's own title, the run's age, a badge.
    #[test]
    fn a_running_row_says_the_title_the_age_and_who_wants_a_human() {
        let untitled = crate::Chat {
            title: Some("   ".into()),
            ..chat("blank", None)
        };
        let chats = vec![chat("named", None), untitled];
        let sessions = vec![
            started("named", crate::SessionStatus::AwaitingInput, "2026-08-01T11:00:00Z"),
            // No `started_at`: the row draws without a counter rather than
            // counting up from the epoch.
            session("blank", crate::SessionStatus::Working, 0),
        ];
        let running = running_rows(&[], &chats, &sessions, None, now());
        assert_eq!(running[0].title, "some title the agent wrote");
        assert_eq!(running[0].elapsed_label(now()).as_deref(), Some("1h00m"));
        assert_eq!(running[1].title, UNTITLED_CHAT);
        assert_eq!(running[1].elapsed_label(now()), None);
        assert_eq!(running_needing_attention(&running), 1);
    }

    /// A box with no board is the case this group matters most on — nothing to
    /// subtract, and the whole live list shows.
    #[test]
    fn no_board_subtracts_nothing() {
        let chats = vec![chat("c1", None)];
        let sessions = vec![session("c1", crate::SessionStatus::Working, 0)];
        assert_eq!(running_rows(&[], &chats, &sessions, None, now()).len(), 1);
    }

    /// The pinned orchestrator has a slot of its own (gh#122) — a working
    /// orchestrator is the slot's news, not a Running row.
    #[test]
    fn the_pinned_orchestrator_is_the_slots_not_runnings() {
        let chats = vec![chat("orch", None), chat("adhoc", None)];
        let sessions = vec![
            session("orch", crate::SessionStatus::Working, 0),
            session("adhoc", crate::SessionStatus::Working, 0),
        ];
        assert_eq!(
            running_rows(&[], &chats, &sessions, Some("orch"), now())
                .iter()
                .map(|r| r.chat_id.as_str())
                .collect::<Vec<_>>(),
            vec!["adhoc"]
        );
    }

    // ---- whose subscription (gh#101) ------------------------------------

    fn login(id: &str, email: &str, harness: crate::HarnessId, active: bool) -> crate::AgentAccount {
        crate::AgentAccount {
            id: id.into(),
            harness,
            email: Some(email.into()),
            plan_label: None,
            active,
            usage_windows: Vec::new(),
            display_name: None,
            organization: None,
            auth_kind: None,
            switchable: true,
            saved_at: None,
        }
    }

    /// A named slot resolves to its own login; naming none resolves to the
    /// box's live one, which is what a dispatch with no `account` really
    /// spends. Both are per harness — a Claude slot cannot pay for a codex run.
    #[test]
    fn billed_email_names_the_slot_or_the_boxs_own_login() {
        use crate::HarnessId::{ClaudeCode, Codex};
        let accounts = vec![
            login("slot-box", "brede@tally.no", ClaudeCode, true),
            login("slot-ana", "ana@example.com", ClaudeCode, false),
            login("slot-cod", "cod@example.com", Codex, true),
        ];
        assert_eq!(
            billed_email(&accounts, ClaudeCode, Some("slot-ana")),
            Some("ana@example.com")
        );
        assert_eq!(
            billed_email(&accounts, ClaudeCode, None),
            Some("brede@tally.no"),
            "no slot named is the box's own login, not nothing"
        );
        assert_eq!(
            billed_email(&accounts, Codex, None),
            Some("cod@example.com")
        );
        // A slot saved for another harness is not this run's to spend.
        assert_eq!(billed_email(&accounts, Codex, Some("slot-ana")), None);
        // Nothing saved at all: the box cannot name whose it is, and says so.
        assert_eq!(billed_email(&[], ClaudeCode, None), None);
    }

    /// The seatbelt's whole comparison. Two unknowns must never read as an
    /// accusation: an unattributed dispatch names nobody to have wronged.
    #[test]
    fn cross_billed_is_a_claim_against_a_slot_email_and_nothing_else() {
        assert!(cross_billed(
            Some("brede@tally.no"),
            Some("ana@example.com")
        ));
        assert!(!cross_billed(
            Some("brede@tally.no"),
            Some("BREDE@Tally.no")
        ));
        assert!(!cross_billed(Some("brede@tally.no"), None));
        assert!(!cross_billed(None, Some("ana@example.com")));
        assert!(!cross_billed(None, None));
        // Empty strings are absence, not a name that differs from everything.
        assert!(!cross_billed(Some("brede@tally.no"), Some("  ")));
    }

    /// A cross-billed attempt says so for its whole life, in the metadata both
    /// viewports draw — working, in review, and long closed.
    #[test]
    fn a_cross_billed_row_says_whose_subscription_it_spends() {
        for state in [BoardState::Working, BoardState::Review, BoardState::Done] {
            let mut r = row("b", state);
            r.billed_to = Some("brede@tally.no".into());
            r.dispatched_by_user = Some("ana@example.com".into());
            r.started_at = Some("2026-08-01T11:59:30Z".into());
            let meta = row_metadata(&r, false, 120, now());
            assert!(
                meta.contains("bills brede@tally.no"),
                "{state:?} row must name the payer: {meta}"
            );
        }

        // The owner releasing their own work is not cross-billed and says
        // nothing extra — the warning has to stay rare enough to mean something.
        let mut own = row("o", BoardState::Working);
        own.billed_to = Some("brede@tally.no".into());
        own.dispatched_by_user = Some("brede@tally.no".into());
        assert_eq!(billing_note(&own), None);
        assert!(!row_metadata(&own, false, 120, now()).contains("bills"));

        // And a row nothing has run on carries no verdict at all.
        assert_eq!(billing_note(&row("r", BoardState::Ready)), None);
    }

    #[test]
    fn the_billing_words_are_the_same_words_everywhere() {
        assert_eq!(bills_label("brede@tally.no"), "bills brede@tally.no");
        assert_eq!(
            bills_warning("brede@tally.no", crate::HarnessId::ClaudeCode),
            "this run bills brede@tally.no's Claude — pass --account <your slot>"
        );
        assert_eq!(
            bills_comment_suffix("brede@tally.no"),
            " · on brede@tally.no's subscription"
        );
        assert_eq!(
            subscription_noun(crate::HarnessId::Codex),
            "Codex",
            "named as its owner thinks of it, not as comet spells the runtime"
        );
    }

    fn device(id: &str, created: &str) -> crate::Device {
        crate::Device {
            id: id.into(),
            name: id.into(),
            platform: "macos".into(),
            last_seen_at: None,
            created_at: Some(
                DateTime::parse_from_rfc3339(created)
                    .unwrap()
                    .with_timezone(&Utc),
            ),
            version: None,
        }
    }

    #[test]
    fn host_sweep_asks_this_device_first_then_the_rest_in_registration_order() {
        let devices = vec![
            device("box", "2026-01-02T00:00:00Z"),
            device("laptop", "2026-01-01T00:00:00Z"),
            device("phone", "2026-01-03T00:00:00Z"),
        ];
        let candidates = host_candidates(&devices, Some("laptop"));
        assert_eq!(
            candidates,
            vec![None, Some("box".into()), Some("phone".into())],
            "this device first, then the others oldest-registered first"
        );
        // The sweep walks the list once and then stops: a board that is
        // nowhere must read as "nowhere", not loop forever.
        assert_eq!(next_host_candidate(&candidates, None), Some(Some("box".into())));
        assert_eq!(
            next_host_candidate(&candidates, Some("box")),
            Some(Some("phone".into()))
        );
        assert_eq!(next_host_candidate(&candidates, Some("phone")), None);
    }

    #[test]
    fn a_device_that_left_mid_sweep_restarts_the_sweep() {
        let devices = vec![device("box", "2026-01-02T00:00:00Z")];
        let candidates = host_candidates(&devices, Some("laptop"));
        assert_eq!(next_host_candidate(&candidates, Some("gone")), Some(None));
    }

    #[test]
    fn a_lone_device_still_asks_itself() {
        assert_eq!(host_candidates(&[], None), vec![None]);
        // Even when the local id is unknown (LocalDevice hasn't answered yet),
        // every registered device is a candidate — asking the box twice under
        // two names is harmless; never asking it is not.
        let devices = vec![device("box", "2026-01-02T00:00:00Z")];
        assert_eq!(
            host_candidates(&devices, None),
            vec![None, Some("box".into())]
        );
    }
}
