//! The comet side of the board — what `herdr.rs` was to herdr-board.
//!
//! herdr-board drove a terminal multiplexer: open a pane, start an agent, read
//! its screen, type into it. comet gives the board structured primitives for
//! every one of those verbs, so this module is a mapping rather than a driver:
//!
//! | herdr-board did                       | comet-board does                        |
//! |---------------------------------------|-----------------------------------------|
//! | `worktree create` + `tab_create` +    | `CreateWorktree` RPC + `Mutate           |
//! | `agent_start`                         | createChat` + `QueueCommand` send       |
//! | `agent_prompt` + nudge-and-verify     | `QueueCommand` send/steer (durable       |
//! |                                       | ledger entry — delivery is guaranteed)   |
//! | screen-scraped `agent_status`         | `Session.status` from the workspace doc  |
//! | `pane_close` on cancel                | `QueueCommand` interrupt + archive chat  |
//! | `HERDR_PANE_ID` provenance            | `COMET_BOARD_CHAT_ID` in the harness env |
//!
//! What has NO equivalent here, deliberately: `screen.rs` (reading terminal
//! contents), `wake.rs` (delivery verification), `nudge.rs` (retrying parked
//! agents). Those existed because herdr's agent state was a heuristic sampled
//! off a screen; comet's is an enum written by the engine that runs the agent.

use chrono::{DateTime, Utc};
use comet_proto::{HarnessId, Session, SessionStatus};
use serde::{Deserialize, Serialize};

use crate::model::AgentStatus;

/// How long a `working` session row may go without an `updated_at` bump before
/// the board stops believing it. Mirrors `comet_proto::view::SESSION_STALE_MS`,
/// the same gate both comet frontends apply — a crashed engine must not hold a
/// board row (and its concurrency slot) forever. This replaces the whole of
/// herdr-board's byte-identical-screen resampling (gh#32).
pub const SESSION_STALE_MS: i64 = comet_proto::view::SESSION_STALE_MS;

/// Runtime names accepted in `routing.toml`, mapped onto comet harnesses.
///
/// herdr accepted twenty agent kinds; comet ships four. The board keeps the
/// operator's spelling for display (a route saying `claude-code` renders as
/// written) and translates only at the point of dispatch, exactly as
/// herdr-board's `herdr_kind_for_runtime` did.
pub fn harness_for_runtime(runtime: &str) -> Option<HarnessId> {
    match runtime {
        "claude-code" | "claude" => Some(HarnessId::ClaudeCode),
        "openai-codex" | "codex" => Some(HarnessId::Codex),
        "cursor" => Some(HarnessId::Cursor),
        // The mock harness is dispatchable on purpose: `demo` and the
        // integration tests release real tasks through the real pipeline.
        "mock" => Some(HarnessId::Mock),
        _ => None,
    }
}

/// The names `harness_for_runtime` accepts, for error messages and `doctor`.
pub const RUNTIME_NAMES: &[&str] = &[
    "claude-code",
    "claude",
    "openai-codex",
    "codex",
    "cursor",
    "mock",
];

/// Derive the board's view of an agent from a comet session row.
///
/// The mapping is deliberately lossy in one direction only: comet has no
/// separate `done` (its Idle covers both), and the board's `Done`/`Idle` split
/// is resolved by the settle logic, not here — a run that *ended* is visible in
/// the run journal, which is the settle authority. See `docs/BOARD.md` §settle.
pub fn agent_status(session: Option<&Session>, now: DateTime<Utc>) -> AgentStatus {
    let Some(s) = session else {
        // No session row for this chat: nothing has ever run in it, or the row
        // was tombstoned with the chat. Same meaning as herdr's vanished pane.
        return AgentStatus::Missing;
    };
    let age_ms = (now - s.updated_at).num_milliseconds();
    match s.status {
        SessionStatus::Working if age_ms > SESSION_STALE_MS => {
            // A Working row nobody is refreshing is a crashed or wedged engine,
            // not a working agent. Unknown, not Idle: staleness is absence of
            // evidence, and the settle logic must not read it as completion.
            AgentStatus::Unknown
        }
        SessionStatus::Working => AgentStatus::Working,
        // The agent asked a question or wants an approval — the board state
        // that exists to say "needs you". First-class here; in herdr this took
        // a hand-written screen-matching rule per agent runtime.
        SessionStatus::AwaitingInput => AgentStatus::Blocked,
        SessionStatus::Idle => AgentStatus::Idle,
        // The run died (harness error, engine recovery stamped `aborted`).
        // Blocked rather than a terminal state for the same reason gh#32 chose
        // it: the chat is intact with its full context, so retry-or-cancel is
        // the operator's call, and the attempt keeps its concurrency slot
        // honestly until they make it.
        SessionStatus::Errored => AgentStatus::Blocked,
    }
}

/// How the last run in a chat ended, as the run journal records it — the
/// board's view of comet's `DoneStatus`.
///
/// This is the fact §H4's settle logic keys off: a run ending is a journal
/// event (`AgentEvent::Done`), not an inference from an idle-looking status,
/// so "the turn ended, now check the checkout" needs no debounce clock. The
/// distinction the *status* mapping cannot make — `Errored` and
/// `AwaitingInput` both read [`AgentStatus::Blocked`] — is exactly the one
/// this preserves: an errored run has ended (and must not settle on commits),
/// a question mid-run has not ended at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunEnd {
    Completed,
    Interrupted,
    Errored,
}

/// Everything a dispatch resolved from a task + its route, expressed in comet
/// vocabulary. The board core produces this; an engine-side executor consumes
/// it. No RPC types leak in here — this crate stays sync and transport-free.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DispatchSpec {
    /// Task identifier, e.g. `LIN-145` or `gh:owner/repo#87` — becomes the
    /// chat title prefix and the provenance the agent sees.
    pub identifier: String,
    /// The space (device + folder pair) the route resolved to. What herdr
    /// called a workspace.
    pub space_id: String,
    /// Host device that owns the space — `Mutate createChat` targets it and
    /// `QueueCommand` executes on it.
    pub device_id: String,
    /// Repo root on the host device, tilde-expanded.
    pub repo_path: String,
    /// Branch to cut, from the route's `branch_template`.
    pub branch: String,
    /// Whether to run in a fresh worktree (the default) or the repo root.
    pub worktree: bool,
    pub harness: HarnessId,
    /// Model override, if the route names one; None = harness default.
    pub model: Option<String>,
    /// The brief: task title, body, links, and the board conventions
    /// (commit as you go, open a PR, `comet-board list --json` to poll).
    /// `{worktree}` may still be unresolved — see [`DispatchSpec::prompt_at`].
    pub prompt: String,
}

impl DispatchSpec {
    /// The brief with `{worktree}` resolved to the checkout the dispatch
    /// actually cut. The one variable resolution cannot fill in advance: the
    /// engine picks the path while executing the spec, so the executor calls
    /// this with the real cwd just before sending the brief.
    pub fn prompt_at(&self, cwd: &str) -> String {
        let mut vars = std::collections::BTreeMap::new();
        vars.insert("worktree", cwd.to_string());
        crate::config::interpolate(&self.prompt, &vars)
    }
}

/// A dispatched attempt, from the board's side of the fence.
///
/// The chat id is the attempt's identity everywhere: stored on the attempt row
/// (herdr-board's `pane_id` column — same slot, new meaning), matched against
/// session rows on reconcile, and exported as `COMET_BOARD_CHAT_ID` to the
/// agent process so `comet-board dispatch` can record provenance without anyone
/// passing ids by hand.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DispatchHandle {
    pub chat_id: String,
    /// Worktree path the attempt runs in (repo root when `worktree` was false).
    pub cwd: String,
}

/// What the board asks of comet. One implementation talks live RPC from inside
/// the engine; tests use a recording fake, as herdr-board's fixture tests did.
///
/// Everything here is a thin verb over an existing comet primitive — the trait
/// exists so `sync`/`dispatch`/`settled` stay testable without an engine, not
/// to abstract over multiple backends.
pub trait Runtime {
    /// Cut the worktree (when asked), create the chat on the host device, and
    /// queue the brief as the first send. Returns once the command entry is
    /// durably in the session doc — NOT once the agent starts: the ledger
    /// guarantees delivery, which is the property herdr-board's
    /// nudge-and-verify loop existed to approximate.
    fn dispatch(&self, spec: &DispatchSpec) -> anyhow::Result<DispatchHandle>;

    /// Deliver text into a live chat — review comments, settle notices to a
    /// dispatching orchestrator. Queued as a steer if a run is live, a send
    /// otherwise; the command ledger's supersede rules apply.
    fn prompt(&self, chat_id: &str, text: &str) -> anyhow::Result<()>;

    /// End the live attempt: queue an interrupt, then archive the chat. The
    /// issue stays open — cancel ends attempts, never tasks.
    fn cancel(&self, chat_id: &str) -> anyhow::Result<()>;

    /// The current session row for a chat, from the workspace-doc mirror.
    /// `None` when no run has ever started or the chat is gone.
    fn session(&self, chat_id: &str) -> anyhow::Result<Option<Session>>;

    /// Whether the chat still exists and is not archived. Reconciliation's
    /// "does the pane still exist" check, minus the screen.
    fn chat_alive(&self, chat_id: &str) -> anyhow::Result<bool>;

    /// Where the chat runs — its row's cwd, recorded at creation. `None` when
    /// the chat is gone or never recorded one. Review delivery (H5) compares
    /// this against the authoring attempt's checkout before delivering: an
    /// operator can re-point a chat at another repo, and a review pasted into
    /// a session that moved on reaches the wrong author. What herdr-board read
    /// off the pane's live cwd, comet states on the chat row.
    fn chat_cwd(&self, chat_id: &str) -> anyhow::Result<Option<String>>;

    /// How the chat's most recent run ended, straight off the run journal:
    /// `Some` when the journal's last event is a `Done`, `None` while a run is
    /// mid-stream (or nothing has ever run). The settle authority §H4 names —
    /// see [`RunEnd`] for why the session status cannot carry this.
    fn last_run_end(&self, chat_id: &str) -> anyhow::Result<Option<RunEnd>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn session(status: SessionStatus, age_ms: i64, now: DateTime<Utc>) -> Session {
        Session {
            chat_id: "chat-1".into(),
            device_id: "dev-1".into(),
            status,
            started_at: None,
            updated_at: now - Duration::milliseconds(age_ms),
        }
    }

    #[test]
    fn missing_session_is_missing() {
        assert_eq!(agent_status(None, Utc::now()), AgentStatus::Missing);
    }

    #[test]
    fn fresh_working_is_working() {
        let now = Utc::now();
        let s = session(SessionStatus::Working, 1_000, now);
        assert_eq!(agent_status(Some(&s), now), AgentStatus::Working);
    }

    #[test]
    fn stale_working_is_unknown_not_idle() {
        let now = Utc::now();
        let s = session(SessionStatus::Working, SESSION_STALE_MS + 1, now);
        assert_eq!(agent_status(Some(&s), now), AgentStatus::Unknown);
    }

    #[test]
    fn awaiting_input_is_blocked() {
        let now = Utc::now();
        let s = session(SessionStatus::AwaitingInput, 1_000, now);
        assert_eq!(agent_status(Some(&s), now), AgentStatus::Blocked);
    }

    #[test]
    fn errored_is_blocked_not_failed() {
        let now = Utc::now();
        let s = session(SessionStatus::Errored, 1_000, now);
        assert_eq!(agent_status(Some(&s), now), AgentStatus::Blocked);
    }

    #[test]
    fn stale_idle_stays_idle() {
        // Staleness only distrusts Working: an idle row not being refreshed is
        // just an idle chat, and Unknown-ing it would flap every settled task.
        let now = Utc::now();
        let s = session(SessionStatus::Idle, SESSION_STALE_MS * 2, now);
        assert_eq!(agent_status(Some(&s), now), AgentStatus::Idle);
    }

    #[test]
    fn runtime_names_all_resolve() {
        for name in RUNTIME_NAMES {
            assert!(harness_for_runtime(name).is_some(), "{name} must resolve");
        }
        assert_eq!(harness_for_runtime("gemini"), None);
    }
}
