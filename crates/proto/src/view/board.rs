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
            // Where it would go, not where it came from: the routed workspace
            // is the routing outcome, otherwise invisible until dispatch.
            let repo = row
                .workspace
                .as_deref()
                .or(row.route.as_deref())
                .unwrap_or_default();
            if !row.dispatchable {
                // A property of the issue, not an affordance for the cursor —
                // so it shows on every such row, selected or not.
                if repo.is_empty() {
                    NO_ROUTE.into()
                } else {
                    format!("{repo} · {NO_ROUTE}")
                }
            } else if selected {
                if repo.is_empty() {
                    "[enter to dispatch]".into()
                } else {
                    format!("{repo} · [enter to dispatch]")
                }
            } else {
                repo.to_string()
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
    fn ready_metadata_names_the_route_and_offers_dispatch_on_selection() {
        let rows = [row("a", BoardState::Ready)];
        // Unselected: the workspace the row would go to.
        assert!(row_metadata(&rows[0], false, 80, now()).contains("offhand"));
        // Selected: the one action the cursor can take.
        assert!(row_metadata(&rows[0], true, 80, now()).contains("[enter to dispatch]"));

        // Nothing routes: the same words the filter uses.
        let mut unrouted = row("u", BoardState::Ready);
        unrouted.route = None;
        unrouted.workspace = None;
        unrouted.dispatchable = false;
        assert_eq!(row_metadata(&unrouted, false, 80, now()), "no route");
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
    }
}
