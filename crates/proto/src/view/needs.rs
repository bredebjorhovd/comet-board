//! The "Needs you" inbox and the orchestrator's pinned slot (gh#122).
//!
//! Every other sidebar section answers "which subsystem produced this row" —
//! space aggregates, session rails, the Agents badge, the finished-unseen
//! green — and each answers with a dot under 8px that carries neither who nor
//! what. This module derives the one section that answers the question a
//! person opening the app actually has: **does anything want me?** Two
//! structures, one derivation each, and they live here for the standing
//! reason everything in [`crate::view`] does — three surfaces draw them (the
//! gpui app, the TUI, the phone), and an inbox whose membership differs
//! between surfaces is a real bug.
//!
//! - [`needs_you`] — the inbox. Everything waiting on a human, as rows that
//!   say WHO and WHAT in words ("Orchestrator · PR 576 green, nothing needs
//!   you" / "gh#503 · agent asks about auth semantics"). First section on
//!   every surface. When it is empty the surface says so in words
//!   ([`ALL_CLEAR`]) — the empty state is the reward, not a gap, and it is
//!   what licenses everything below it to stay calm forever.
//! - [`orchestrator_slot`] — the board's voice as a pinned thread above
//!   Spaces: name, unseen badge, latest-report preview. A fixture like a
//!   messaging app's pinned conversation, not a decorated session row; when
//!   the orchestrator has never spoken the slot says so ([`NO_REPORTS`])
//!   instead of vanishing, teaching where to look before the first notice
//!   arrives.

use chrono::{DateTime, Utc};

use crate::view::board::{AgentState, TaskRow, agent_state};
use crate::view::{Indicator, display_status, effective_indicator, single_line};
use crate::{Chat, ChatIndicator, Session};

/// What every surface calls the pinned orchestrator when it speaks for
/// itself — the inbox row's WHO and the slot's name. One spelling, everywhere:
/// this is the product's voice, and a voice with three names is three voices.
pub const ORCHESTRATOR_NAME: &str = "Orchestrator";

/// The section's title, pinned so no surface invents its own.
pub const NEEDS_YOU_TITLE: &str = "Needs you";

/// The empty state, in words. A quiet check — not an empty gap, and never an
/// omitted section: the promise "this section cannot miss" is only worth
/// anything if its silence is spelled out too.
pub const ALL_CLEAR: &str = "Nothing needs you";

/// What the slot says under the name when the pinned orchestrator has never
/// spoken. An empty fixture, deliberately rendered: it teaches where the first
/// notice will arrive before one ever has.
pub const NO_REPORTS: &str = "No reports yet";

/// Why a row is in the inbox — three ways a human ends up owed something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NeedKind {
    /// Somebody is waiting on your answer: a question, or a permission prompt.
    Question,
    /// A run died and is waiting on a retry.
    DeadRun,
    /// The orchestrator finished a turn you have not seen.
    Report,
}

impl NeedKind {
    /// Inbox order — lower first. The same judgement [`AgentState::rank`]
    /// encodes: a question outranks a corpse, and both outrank news.
    pub fn rank(self) -> u8 {
        match self {
            NeedKind::Question => 0,
            NeedKind::DeadRun => 1,
            NeedKind::Report => 2,
        }
    }

    /// The board's own shape families (`view::board::BoardState::glyph`), so a
    /// row means the same thing here as one section down: pointed is stuck,
    /// crossed is dead, checked is done-and-for-you.
    pub fn glyph(self) -> &'static str {
        match self {
            NeedKind::Question => "▲",
            NeedKind::DeadRun => "✕",
            NeedKind::Report => "✓",
        }
    }

    /// What the row says when the chat has no last words to quote.
    pub fn fallback(self) -> &'static str {
        match self {
            NeedKind::Question => "waiting on your answer",
            NeedKind::DeadRun => "its run died — open to retry",
            NeedKind::Report => "finished a turn you haven't seen",
        }
    }
}

/// One thing waiting on a human, as every surface draws it: who, and one line
/// of what, in words.
#[derive(Debug, Clone, PartialEq)]
pub struct NeedRow {
    /// Where opening the row goes. Always a chat that exists here — a need
    /// whose chat has not synced cannot be answered from this device, and a
    /// row that cannot be opened is a dead end wearing an alert.
    pub chat_id: String,
    /// The space to land in, as the chat records it.
    pub space_id: Option<String>,
    /// WHO wants you: [`ORCHESTRATOR_NAME`], an issue identifier (`gh#503`),
    /// or the chat's own title.
    pub who: String,
    /// A short slug of the task's title (gh#364), drawn after
    /// [`who`](Self::who) where the room is there — `gh#503
    /// review-page-loads`.
    ///
    /// Only ever set on the rows whose `who` is an identifier: the
    /// orchestrator's row and an ad-hoc chat's are already named in words, and
    /// a slug of the chat title beside the chat title says one thing twice.
    /// Decoration on the key, so a narrow surface drops it and keeps the
    /// identifier — see [`crate::view::board::AgentRow::slug`].
    pub slug: Option<String>,
    /// WHAT it wants, one line: the chat's last words
    /// (`last_message_preview`), or the kind's [`NeedKind::fallback`].
    pub what: String,
    pub kind: NeedKind,
    /// When this started waiting, best known. The instant rather than the age
    /// so a viewport re-reads the clock on its own frames — the rule every
    /// sidebar counter here follows.
    pub since: Option<DateTime<Utc>>,
}

/// Everything waiting on a human, most owed first.
///
/// The inputs are the four standing streams every viewport already holds: the
/// orchestrator pin, `WatchBoard` rows, the chat rows, the session watch.
/// Membership, exactly:
///
/// - **The orchestrator**, when its turn finished unseen ([`NeedKind::Report`])
///   — the one chat whose *news* is worth a row, because its news is the
///   board's. Its questions and dead runs qualify like anyone else's.
/// - **Live board attempts** whose agent is blocked or dead, state read
///   session-first exactly as the Agents section reads it. WHO is the issue
///   identifier — what the agent is *for*.
/// - **Any other chat that is asking** (`AwaitingInput`, staleness-gated).
///   A finished ad-hoc session's green dot stays the sessions list's news,
///   and a dead ad-hoc run stays at the recency it earned — only the
///   orchestrator's news interrupts, or the section becomes a second
///   sessions list and stops meaning "you are needed".
///
/// Each chat appears at most once: the orchestrator's row wins over its
/// attempt row (if a board ever dispatched it), and an attempt's chat is
/// never re-listed as a plain asking chat.
///
/// Order: [`NeedKind::rank`], then longest-waiting first — the row that has
/// been owed the longest is the most owed — with `since`-less rows last and
/// the chat id breaking the final tie so the sort is total.
pub fn needs_you(
    orchestrator: Option<&str>,
    rows: &[TaskRow],
    chats: &[Chat],
    sessions: &[Session],
    now: DateTime<Utc>,
) -> Vec<NeedRow> {
    let session_for = |id: &str| sessions.iter().find(|s| s.chat_id == id);
    let mut out: Vec<NeedRow> = Vec::new();

    // The orchestrator, by every door: question, dead run, unseen report.
    if let Some(pin) = orchestrator
        && let Some(chat) = chats.iter().find(|c| c.id == pin)
    {
        let session = session_for(pin);
        let kind = match display_status(chat, session, now) {
            ChatIndicator::AwaitingInput => Some(NeedKind::Question),
            ChatIndicator::Errored => Some(NeedKind::DeadRun),
            ChatIndicator::Completed => Some(NeedKind::Report),
            ChatIndicator::Working | ChatIndicator::Idle => None,
        };
        if let Some(kind) = kind {
            out.push(NeedRow {
                chat_id: chat.id.clone(),
                space_id: chat.space_id.clone(),
                who: ORCHESTRATOR_NAME.to_string(),
                // A name already, and not a task's — nothing to decorate.
                slug: None,
                what: what_line(chat, kind),
                kind,
                since: match kind {
                    // When it spoke, for news; when it stalled, for the rest.
                    NeedKind::Report => chat.last_message_at,
                    _ => session.map(|s| s.updated_at).or(chat.last_message_at),
                },
            });
        }
    }

    // Live board attempts whose agent wants a human. The chat-must-exist rule
    // is `agent_rows`'s, for the same reason: a row that cannot be opened is
    // not an inbox row.
    for row in rows.iter().filter(|row| row.state().holds_pane()) {
        let Some(chat_id) = row.chat_id.as_deref() else {
            continue;
        };
        if orchestrator == Some(chat_id) {
            continue;
        }
        let Some(chat) = chats.iter().find(|c| c.id == chat_id) else {
            continue;
        };
        let session = session_for(chat_id);
        let kind = match agent_state(row.state(), session, now) {
            AgentState::Blocked => NeedKind::Question,
            AgentState::Errored => NeedKind::DeadRun,
            AgentState::Working => continue,
        };
        // When the live session drove the verdict, its last transition is when
        // the waiting started; a board-only verdict knows the attempt's start
        // at best.
        let since = session
            .filter(|s| effective_indicator(Some(s), now) != Indicator::None)
            .map(|s| s.updated_at)
            .or_else(|| {
                row.started_at
                    .as_deref()
                    .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
                    .map(|at| at.with_timezone(&Utc))
            });
        out.push(NeedRow {
            chat_id: chat_id.to_string(),
            space_id: chat.space_id.clone(),
            who: row.identifier.clone(),
            slug: row.slug(),
            what: what_line(chat, kind),
            kind,
            since,
        });
    }

    // Everything else that is asking. Archived is not a reason to hide a
    // question — the rule the Running group already keeps.
    let claimed: Vec<&str> = rows
        .iter()
        .filter(|row| row.state().holds_pane())
        .filter_map(|row| row.chat_id.as_deref())
        .collect();
    for chat in chats {
        if orchestrator == Some(chat.id.as_str()) || claimed.contains(&chat.id.as_str()) {
            continue;
        }
        let session = session_for(&chat.id);
        if effective_indicator(session, now) != Indicator::AwaitingInput {
            continue;
        }
        out.push(NeedRow {
            chat_id: chat.id.clone(),
            space_id: chat.space_id.clone(),
            who: chat
                .title
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .unwrap_or(crate::view::board::UNTITLED_CHAT)
                .to_string(),
            // Named in its own words already — see [`NeedRow::slug`].
            slug: None,
            what: what_line(chat, NeedKind::Question),
            kind: NeedKind::Question,
            since: session.map(|s| s.updated_at),
        });
    }

    out.sort_by(|a, b| {
        a.kind
            .rank()
            .cmp(&b.kind.rank())
            .then_with(|| match (a.since, b.since) {
                (Some(a), Some(b)) => a.cmp(&b),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
            .then_with(|| a.chat_id.cmp(&b.chat_id))
    });
    out
}

/// One line of WHAT: the chat's last words, else the kind's own words.
fn what_line(chat: &Chat, kind: NeedKind) -> String {
    chat.last_message_preview
        .as_deref()
        .map(single_line)
        .filter(|preview| !preview.is_empty())
        .unwrap_or_else(|| kind.fallback().to_string())
}

// ---------------------------------------------------------------------------
// The orchestrator's pinned slot
// ---------------------------------------------------------------------------

/// The orchestrator as a pinned thread: what the fixed slot above Spaces
/// renders on every surface.
#[derive(Debug, Clone, PartialEq)]
pub struct OrchestratorSlot {
    /// The pinned chat — where opening the slot goes. Opening marks the chat
    /// seen (the synced LWW marker), which is what clears [`unseen`]
    /// everywhere.
    ///
    /// [`unseen`]: OrchestratorSlot::unseen
    pub chat_id: String,
    pub space_id: Option<String>,
    /// The latest report's one-line preview. `None` = the orchestrator has
    /// never spoken, and the slot says [`NO_REPORTS`] instead of vanishing.
    pub preview: Option<String>,
    /// Unread activity — the slot's badge. A word or a count on the surface,
    /// never only a color.
    pub unseen: bool,
    /// Live status, so the slot can say "working" while a turn runs — in the
    /// screenshots an 8h-old orchestrator row was indistinguishable from an
    /// idle one, and this is the field that fixes it.
    pub indicator: ChatIndicator,
    /// When it last spoke, for the slot's time column.
    pub last_at: Option<DateTime<Utc>>,
}

/// The slot, or `None` when this board has no orchestrator pinned (or its
/// chat has not synced here — a slot that cannot be opened teaches nothing).
/// A pinned-but-silent orchestrator is NOT `None`: that is the empty fixture,
/// and it renders.
pub fn orchestrator_slot(
    orchestrator: Option<&str>,
    chats: &[Chat],
    sessions: &[Session],
    now: DateTime<Utc>,
) -> Option<OrchestratorSlot> {
    let pin = orchestrator?;
    let chat = chats.iter().find(|c| c.id == pin)?;
    let session = sessions.iter().find(|s| s.chat_id == pin);
    Some(OrchestratorSlot {
        chat_id: chat.id.clone(),
        space_id: chat.space_id.clone(),
        preview: chat
            .last_message_preview
            .as_deref()
            .map(single_line)
            .filter(|preview| !preview.is_empty()),
        unseen: chat.unseen(),
        indicator: display_status(chat, session, now),
        last_at: chat.last_message_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionStatus;
    use crate::view::board::BoardState;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-08-07T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn at(rfc: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn chat(id: &str, title: Option<&str>, preview: Option<&str>) -> Chat {
        Chat {
            id: id.into(),
            device_id: "box".into(),
            title: title.map(str::to_string),
            archived: false,
            cwd: None,
            branch: None,
            checkout_id: None,
            config: None,
            last_message_preview: preview.map(str::to_string),
            last_message_at: preview.map(|_| at("2026-08-07T11:00:00Z")),
            created_at: at("2026-08-01T00:00:00Z"),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: Some("space".into()),
            last_seen_at: None,
        }
    }

    fn session(chat_id: &str, status: SessionStatus, age_ms: i64) -> Session {
        Session {
            chat_id: chat_id.into(),
            device_id: "box".into(),
            status,
            started_at: None,
            updated_at: now() - chrono::Duration::milliseconds(age_ms),
        }
    }

    fn attempt(id: &str, chat_id: &str, state: BoardState) -> TaskRow {
        TaskRow {
            id: id.into(),
            identifier: format!("gh#{id}"),
            title: format!("task {id}"),
            state: state.as_str().into(),
            source: "github".into(),
            url: String::new(),
            labels: vec![],
            dispatchable: true,
            gone: false,
            route: None,
            workspace: None,
            runtime: None,
            chat_id: Some(chat_id.into()),
            preparation: None,
            review_chat_id: Some(chat_id.into()),
            pr_url: None,
            pr_number: None,
            pr_base_ref: None,
            pr_mergeable: None,
            changes_below: None,
            landing: None,
            stack: None,
            branch: None,
            dispatched_by: None,
            dispatched_by_chat: None,
            last_outcome: None,
            last_outcome_at: None,
            attempts: 1,
            reopened: 0,
            updated_at: "2026-08-07T10:00:00Z".into(),
            started_at: Some("2026-08-07T09:00:00Z".into()),
            account: None,
            dispatched_by_user: None,
            dispatched_by_verified: false,
            billed_to: None,
            max_duration_secs: None,
            context: None,
        }
    }

    /// The exit criterion, literally: the orchestrator has reported and one
    /// agent is blocked — two rows, each saying who and what in words, the
    /// question first.
    #[test]
    fn a_report_and_a_blocked_agent_are_two_rows_that_name_themselves() {
        let orchestrator = chat(
            "orch",
            Some("Board orchestrator"),
            Some("PR 576 green, nothing needs you"),
        );
        let agent_chat = chat("c503", None, Some("agent asks about auth semantics"));
        let rows = vec![attempt("503", "c503", BoardState::Working)];
        let sessions = vec![session("c503", SessionStatus::AwaitingInput, 0)];

        let needs = needs_you(
            Some("orch"),
            &rows,
            &[orchestrator, agent_chat],
            &sessions,
            now(),
        );
        assert_eq!(needs.len(), 2);
        assert_eq!(needs[0].who, "gh#503");
        assert_eq!(needs[0].what, "agent asks about auth semantics");
        assert_eq!(needs[0].kind, NeedKind::Question);
        assert_eq!(needs[1].who, ORCHESTRATOR_NAME);
        assert_eq!(needs[1].what, "PR 576 green, nothing needs you");
        assert_eq!(needs[1].kind, NeedKind::Report);
    }

    /// Nothing pending derives to nothing — and the surfaces render
    /// [`ALL_CLEAR`] in words rather than omitting the section.
    #[test]
    fn nothing_pending_is_empty_not_missing() {
        let working = chat("w", Some("busy"), None);
        let idle = chat("i", Some("quiet"), None);
        let sessions = vec![session("w", SessionStatus::Working, 0)];
        assert!(needs_you(None, &[], &[working, idle], &sessions, now()).is_empty());
    }

    /// Membership edges for plain chats: only a live AwaitingInput qualifies.
    /// Completed and errored ad-hoc sessions stay the sessions list's news;
    /// stale questions are dead backends, not questions.
    #[test]
    fn only_the_orchestrators_news_interrupts() {
        let asking = chat("ask", Some("ad-hoc run"), Some("which env?"));
        let finished = chat("done", Some("finished thing"), Some("all done"));
        let mut errored = chat("err", Some("crashed thing"), None);
        errored.last_message_at = Some(at("2026-08-07T11:00:00Z"));
        let stale = chat("stale", Some("dead backend"), None);
        let sessions = vec![
            session("ask", SessionStatus::AwaitingInput, 0),
            session("err", SessionStatus::Errored, 0),
            session(
                "stale",
                SessionStatus::AwaitingInput,
                crate::view::SESSION_STALE_MS + 1_000,
            ),
        ];
        let needs = needs_you(
            None,
            &[],
            &[asking, finished, errored, stale],
            &sessions,
            now(),
        );
        assert_eq!(
            needs
                .iter()
                .map(|n| (n.who.as_str(), n.kind))
                .collect::<Vec<_>>(),
            vec![("ad-hoc run", NeedKind::Question)]
        );
        assert_eq!(needs[0].what, "which env?");
    }

    /// A blocked or dead board agent is a row named by its issue; a working
    /// one is not in the inbox at all.
    #[test]
    fn board_agents_enter_by_state_and_are_named_by_their_issue() {
        let chats = vec![
            chat("c1", None, None),
            chat("c2", None, None),
            chat("c3", None, None),
        ];
        let rows = vec![
            attempt("a", "c1", BoardState::Working),
            attempt("b", "c2", BoardState::Working),
            attempt("c", "c3", BoardState::Blocked),
        ];
        let sessions = vec![
            session("c1", SessionStatus::Working, 0),
            session("c2", SessionStatus::Errored, 0),
            // c3: no session — the board's blocked verdict stands in.
        ];
        let needs = needs_you(None, &rows, &chats, &sessions, now());
        assert_eq!(
            needs
                .iter()
                .map(|n| (n.who.as_str(), n.kind))
                .collect::<Vec<_>>(),
            vec![("gh#c", NeedKind::Question), ("gh#b", NeedKind::DeadRun)]
        );
        // No last words to quote: the kind speaks.
        assert_eq!(needs[0].what, NeedKind::Question.fallback());
        assert_eq!(needs[1].what, NeedKind::DeadRun.fallback());
    }

    /// One chat, one row: the orchestrator's row wins over its attempt row,
    /// and an attempt's chat is never re-listed as a plain asking chat.
    #[test]
    fn each_chat_appears_at_most_once() {
        let chats = vec![chat("orch", Some("orchestrator"), Some("hello"))];
        let rows = vec![attempt("x", "orch", BoardState::Blocked)];
        let sessions = vec![session("orch", SessionStatus::AwaitingInput, 0)];
        let needs = needs_you(Some("orch"), &rows, &chats, &sessions, now());
        assert_eq!(needs.len(), 1);
        assert_eq!(needs[0].who, ORCHESTRATOR_NAME);
    }

    /// A blocked attempt whose chat has not synced here cannot be opened, so
    /// it is not an inbox row — the Agents/board panes still carry it.
    #[test]
    fn a_need_without_a_chat_here_is_dropped() {
        let rows = vec![attempt("x", "elsewhere", BoardState::Blocked)];
        assert!(needs_you(None, &rows, &[], &[], now()).is_empty());
    }

    /// Questions first, then dead runs, then news; longest-waiting first
    /// within a kind.
    #[test]
    fn most_owed_first() {
        let chats = vec![
            chat("q-new", Some("new question"), None),
            chat("q-old", Some("old question"), None),
            chat("orch", Some("orchestrator"), Some("report")),
        ];
        let sessions = vec![
            session("q-new", SessionStatus::AwaitingInput, 1_000),
            session("q-old", SessionStatus::AwaitingInput, 30_000),
        ];
        let needs = needs_you(Some("orch"), &[], &chats, &sessions, now());
        assert_eq!(
            needs.iter().map(|n| n.chat_id.as_str()).collect::<Vec<_>>(),
            vec!["q-old", "q-new", "orch"]
        );
    }

    // ---- the slot ---------------------------------------------------------

    #[test]
    fn the_slot_is_the_pinned_chat_with_its_last_report_and_badge() {
        let orch = chat(
            "orch",
            Some("whatever it calls itself"),
            Some("PR 576 green"),
        );
        let sessions = vec![session("orch", SessionStatus::Working, 0)];
        let slot = orchestrator_slot(Some("orch"), std::slice::from_ref(&orch), &sessions, now())
            .expect("pinned and synced");
        assert_eq!(slot.chat_id, "orch");
        assert_eq!(slot.preview.as_deref(), Some("PR 576 green"));
        assert!(slot.unseen, "spoke, never opened — the badge shows");
        assert_eq!(slot.indicator, ChatIndicator::Working);
        assert_eq!(slot.last_at, orch.last_message_at);
    }

    /// Opening the chat (the synced seen marker) clears the badge everywhere.
    #[test]
    fn opening_clears_the_badge() {
        let mut orch = chat("orch", None, Some("PR 576 green"));
        orch.last_seen_at = Some(at("2026-08-07T11:30:00Z"));
        let slot = orchestrator_slot(Some("orch"), &[orch], &[], now()).unwrap();
        assert!(!slot.unseen);
    }

    /// Never spoken: the slot still renders, as the empty fixture.
    #[test]
    fn a_silent_orchestrator_is_a_slot_with_no_preview_not_no_slot() {
        let orch = chat("orch", None, None);
        let slot = orchestrator_slot(Some("orch"), &[orch], &[], now()).unwrap();
        assert_eq!(slot.preview, None);
        assert!(!slot.unseen);
        assert_eq!(slot.indicator, ChatIndicator::Idle);
    }

    /// No pin, or a pin whose chat has not synced here: no slot.
    #[test]
    fn no_pin_or_no_chat_is_no_slot() {
        assert!(orchestrator_slot(None, &[chat("c", None, None)], &[], now()).is_none());
        assert!(orchestrator_slot(Some("gone"), &[chat("c", None, None)], &[], now()).is_none());
    }

    /// A multi-line preview collapses onto the slot's one line — same rule as
    /// every single-line surface.
    #[test]
    fn previews_are_single_lines() {
        let orch = chat("orch", None, Some("line one\nline two"));
        let slot = orchestrator_slot(Some("orch"), &[orch.clone()], &[], now()).unwrap();
        assert_eq!(slot.preview.as_deref(), Some("line one line two"));
        let needs = needs_you(Some("orch"), &[], &[orch], &[], now());
        assert_eq!(needs[0].what, "line one line two");
    }
}
