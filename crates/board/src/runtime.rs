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
        "opencode" => Some(HarnessId::Opencode),
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
    "opencode",
    "mock",
];

/// One runtime a dispatch can be pointed at, as a picker renders it.
///
/// `name` is exactly what `routing.toml` and the `DispatchTask` override
/// accept; `label` is the human spelling a picker shows. Served to the
/// frontends by the engine's `ListBoardRuntimes`, so the board panel and the
/// CLI offer the same set the engine validates against.
///
/// `harness` is what [`harness_for_runtime`] resolves the name to. It rides
/// along so an account picker can tell which saved logins a runtime can spend
/// (gh#74) without either frontend re-implementing the mapping — a Claude slot
/// is not lendable to a codex run, and the two config-dir variables are not
/// interchangeable.
///
/// The shape lives in proto (`comet_proto::view::board`) so the viewports can
/// deserialize `ListBoardRuntimes` without depending on this crate, exactly as
/// [`crate::rows::TaskRow`] does; what the list *contains* is this module's.
pub use comet_proto::view::board::RuntimeOption;

/// Why a runtime cannot start on a given device (gh#187) — re-exported beside
/// [`RuntimeOption`] because it is the field that turned that list from a
/// constant into an answer about a box.
pub use comet_proto::view::board::RuntimeUnavailable;

/// The runtimes a dispatch can be told to use, in picker order.
///
/// One canonical name per harness — the aliases `RUNTIME_NAMES` also accepts
/// (`claude`, `openai-codex`) are config spelling, not things to offer: a
/// route saying `openai-codex` still dispatches to the codex harness, and a
/// picker offering both would be offering the same thing twice. `mock` is
/// listed because it is dispatchable on purpose (`demo`, integration tests).
///
/// Every entry comes back [`RuntimeOption::available`]: which names are
/// *spellable* is a property of the board, and this crate has no view of any
/// device. Whether each could actually start is stamped on by whoever answers
/// for a device — `comet_engine::runtimes` (gh#187) — which is also what
/// [`Runtime::harness_availability`] asks before a dispatch cuts anything.
pub fn runtime_options() -> Vec<RuntimeOption> {
    use HarnessId::*;
    [
        (ClaudeCode, "Claude Code"),
        (Opencode, "OpenCode"),
        (Codex, "Codex"),
        (Cursor, "Cursor"),
        (Mock, "Mock"),
    ]
    .into_iter()
    .map(|(id, label)| RuntimeOption {
        name: runtime_name(id).to_string(),
        label: label.to_string(),
        harness: id,
        unavailable: None,
    })
    .collect()
}

/// The canonical runtime name for a harness — the one a picker offers and a
/// dispatch override sends. The reverse of [`harness_for_runtime`], restricted
/// to the canonical spelling (no aliases).
pub fn runtime_name(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::ClaudeCode => "claude-code",
        HarnessId::Codex => "codex",
        HarnessId::Cursor => "cursor",
        HarnessId::Opencode => "opencode",
        HarnessId::Mock => "mock",
    }
}

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
    /// The task's own title, for [`DispatchSpec::chat_title`]. Carried on the
    /// spec rather than looked up at rename time because the engine that names
    /// the chat has no tracker and no board table to ask.
    pub title: String,
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
    /// The ref that branch is cut from, from the route's `base` (gh#67).
    /// `origin/HEAD` by default — the executor fetches it before cutting, so
    /// what an agent starts on is the remote's tip and not whatever the space
    /// folder happened to be sitting on. `HEAD` is the local-checkout opt-out.
    /// Ignored on a retry, which reuses the previous attempt's branch as-is.
    pub base: String,
    /// Whether to run in a fresh worktree (the default) or the repo root.
    pub worktree: bool,
    pub harness: HarnessId,
    /// Model override, if the route names one; None = harness default.
    pub model: Option<String>,
    /// Agent-account slot id the run spends (gh#59) — the dispatch's choice,
    /// else the route's, else `None` for the device's own CLI login. The
    /// executor materializes it into a config dir and points the harness child
    /// at that instead of swapping the shared one; the board core only carries
    /// the id, since which logins exist is engine knowledge.
    pub account: Option<String>,
    /// `owner/repo` the attempt's branch belongs to (gh#68) — what the agent's
    /// `git push` and `gh pr create` authenticate against, minted per use from
    /// the board's GitHub App rather than taken from the box user's git
    /// credentials. From the task id for a GitHub ticket, and from the
    /// checkout's `origin` remote for anything else. `None` when the space has
    /// no GitHub remote at all, which is the case that keeps its own
    /// credentials.
    pub push_repo: Option<String>,
    /// Who the attempt's commits are by (gh#107) — the `[users]` entry for
    /// whoever released it, when this board has one. The engine stamps it on
    /// the harness child as `GIT_AUTHOR_*`, leaving the box's pinned identity
    /// as the committer, so a teammate's dispatch produces commits GitHub
    /// attributes to the teammate. `None` — an operator the map does not name,
    /// or a board that keeps no map — authors as the box, as it always did.
    pub git_author: Option<comet_proto::GitAuthor>,
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

    /// What the chat is called in the sidebar: `gh#25 · D1 Prototype v1`.
    ///
    /// The identifier alone is what shipped, and a shelf of `gh#10 gh#25 gh#26
    /// gh#11` is a list you have to look each row up to read (operator,
    /// 2026-08-08). It stays, and stays in front: it is short, it is what the
    /// board rows and the branch sub-line say, and it is therefore the part
    /// that must survive a narrow pane — the surfaces elide from the right, so
    /// the recognisable half goes on the left and the descriptive half takes
    /// whatever width is left.
    ///
    /// The title is capped at [`CHAT_TITLE_MAX`] graphemes-by-`char` so one
    /// essay-length issue cannot make a chat name nothing else fits beside; an
    /// empty title falls back to the bare identifier rather than leaving a
    /// dangling separator.
    pub fn chat_title(&self) -> String {
        let title = self.title.trim();
        if title.is_empty() {
            return self.identifier.clone();
        }
        format!("{} · {}", self.identifier, truncate_title(title))
    }
}

/// How much task title a chat name carries. Long enough for a real sentence,
/// short enough that the name is still a name.
const CHAT_TITLE_MAX: usize = 60;

fn truncate_title(title: &str) -> String {
    if title.chars().count() <= CHAT_TITLE_MAX {
        return title.to_string();
    }
    // Cut on a word boundary when there is one near the end, so the name reads
    // as a clipped sentence rather than a clipped word.
    let cut: String = title.chars().take(CHAT_TITLE_MAX).collect();
    let stem = match cut.rfind(char::is_whitespace) {
        Some(ix) if ix >= CHAT_TITLE_MAX / 2 => &cut[..ix],
        _ => cut.as_str(),
    };
    format!("{}…", stem.trim_end_matches([' ', ',', ':', ';', '-', '—']))
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

    /// Reclaim a finished attempt's checkout and the local branch it was cut
    /// on (gh#72) — herdr-board's `gc`, which the port left behind.
    ///
    /// `repo_path` is where the worktree came from, recorded at dispatch: the
    /// removal, the prune and the `branch -D` all run there, and it is the one
    /// fact that survives the checkout being deleted by hand. `None` means an
    /// attempt older than that column, and the engine derives what it can from
    /// the checkout instead. `branch` is the branch the *board* cut; the
    /// implementation deletes it only if the checkout is still on it, because
    /// an operator may have checked out something else in there.
    ///
    /// Best-effort by contract: a checkout already gone, a branch already
    /// deleted and a repo that has moved are all `Ok(())`. An `Err` means the
    /// board should try again next cycle rather than record the space as
    /// reclaimed.
    ///
    /// The default is a refusal rather than a no-op: a runtime that cannot do
    /// this must not have the board stamping checkouts as collected that are
    /// still sitting on the disk.
    fn reclaim_worktree(
        &self,
        repo_path: Option<&str>,
        worktree: &str,
        branch: Option<&str>,
    ) -> anyhow::Result<()> {
        let _ = (repo_path, worktree, branch);
        anyhow::bail!("this runtime cannot reclaim worktrees")
    }

    /// Delete the build output inside a finished attempt's checkout (gh#186) —
    /// `target/`, `node_modules/` and the rest of
    /// [`crate::gc::BUILD_OUTPUT_DIRS`] — leaving the checkout itself alone.
    ///
    /// The counterpart to [`Runtime::reclaim_worktree`] and emphatically not a
    /// weaker version of it: that one hands the whole directory and its branch
    /// back, this one removes a cache from inside a checkout that stays, on its
    /// branch, ready for the next `cargo build` and for the agent review delivery
    /// resumes in it. A checkout is 14 MB and its build output is 20–36 GB, which
    /// is why they cannot share a clock.
    ///
    /// Through the runtime rather than done in place for
    /// [`Runtime::reclaim_worktree`]'s reason: the process that owns the
    /// worktrees is the one that may delete inside them, and a read-only board
    /// process is welcome to keep the clock and sweep nothing.
    ///
    /// Best-effort and idempotent by contract: a checkout that is gone, or one
    /// nothing was ever built in, is an empty [`crate::gc::Swept`]. Directories
    /// that would not delete come back in [`crate::gc::Swept::failed`] rather
    /// than as an `Err` — one unreadable directory must not keep the other 30 GB.
    ///
    /// The default is a refusal rather than a no-op, like
    /// [`Runtime::reclaim_worktree`]'s: a runtime that cannot do this must not
    /// have the board recording caches as swept that are still on the disk.
    fn reclaim_build_output(&self, worktree: &str) -> anyhow::Result<crate::gc::Swept> {
        let _ = worktree;
        anyhow::bail!("this runtime cannot sweep build output")
    }

    /// Put a chat on or off its space's shelf (gh#139) — the archive half of
    /// [`Runtime::cancel`], without the interrupt.
    ///
    /// `cancel` archives because the attempt is being ended; this archives
    /// because the attempt ended a week ago and nobody is coming back to the
    /// conversation. Nothing is deleted either way: an archived chat keeps its
    /// transcript, Settings → Archived puts it back, and the board un-archives
    /// one itself if the attempt it belongs to is re-opened.
    ///
    /// Idempotent by contract — archiving an archived chat is `Ok(())`, and so
    /// is un-archiving a live one — because the board's record of what it
    /// archived and the shelf itself are two states that can drift.
    ///
    /// The default is a refusal rather than a no-op, like
    /// [`Runtime::reclaim_worktree`]'s: a runtime that cannot do this must not
    /// have the board recording chats as archived that are still on the shelf.
    fn set_chat_archived(&self, chat_id: &str, archived: bool) -> anyhow::Result<()> {
        let _ = (chat_id, archived);
        anyhow::bail!("this runtime cannot archive chats")
    }

    /// Whose subscription a dispatch on `account` would spend, as an email
    /// (gh#101).
    ///
    /// `None` for `account` is the device's own CLI login — the *active* one
    /// for that harness, which is exactly what a run naming no slot reaches.
    /// The board core cannot answer this itself: which logins a device has
    /// saved, and which of them is live, is engine knowledge, and the board
    /// carries only the slot id (see [`DispatchSpec::account`]).
    ///
    /// The default is `Ok(None)` rather than a refusal, unlike
    /// [`Runtime::reclaim_worktree`]'s: a runtime that cannot name the login
    /// leaves the guard with nothing to compare, and the guard's answer to
    /// "I do not know whose this is" must be silence, never an accusation.
    fn account_email(
        &self,
        harness: HarnessId,
        account: Option<&str>,
    ) -> anyhow::Result<Option<String>> {
        let _ = (harness, account);
        Ok(None)
    }

    /// Could a dispatch on `harness` actually start on this runtime's device
    /// (gh#187)? `Ok(None)` is yes.
    ///
    /// Asked before the attempt row exists, for exactly the reason
    /// [`crate::runtime::DispatchSpec::account`] is resolved before the chat
    /// is: a dispatch that gets as far as a worktree and a chat before
    /// discovering the CLI is not installed has spent the expensive part on the
    /// cheap fact, and left a row somebody has to clean up.
    ///
    /// `account` is the slot the dispatch would spend, so an implementation
    /// can tell "this device's own login is missing" from "this run reads a
    /// slot's login and never the device's".
    ///
    /// The default is `Ok(None)` rather than a refusal — the [`account_email`]
    /// rule, not the [`reclaim_worktree`] one. A runtime that cannot say which
    /// harnesses its device has must not be the reason a legitimate dispatch is
    /// refused; the harness itself still fails the run, loudly, the way it
    /// always did.
    ///
    /// [`account_email`]: Runtime::account_email
    /// [`reclaim_worktree`]: Runtime::reclaim_worktree
    fn harness_availability(
        &self,
        harness: HarnessId,
        account: Option<&str>,
    ) -> anyhow::Result<Option<RuntimeUnavailable>> {
        let _ = (harness, account);
        Ok(None)
    }

    /// How the chat's most recent run ended, straight off the run journal:
    /// `Some` when the journal's last event is a `Done`, `None` while a run is
    /// mid-stream (or nothing has ever run). The settle authority §H4 names —
    /// see [`RunEnd`] for why the session status cannot carry this.
    fn last_run_end(&self, chat_id: &str) -> anyhow::Result<Option<RunEnd>>;

    /// What the chat has spent so far, summed off the same run journal
    /// (gh#151). `None` is "nothing reported" — the board leaves the attempt's
    /// token columns NULL for it, and the stats page renders a blank.
    ///
    /// Default `Ok(None)` rather than a refusal, unlike
    /// [`Runtime::reclaim_worktree`]'s: a runtime that cannot count tokens
    /// costs the page some coverage, and coverage is a number the page already
    /// reports honestly. Nothing is at stake in being silent here.
    fn run_tokens(&self, chat_id: &str) -> anyhow::Result<Option<RunTokens>> {
        let _ = chat_id;
        Ok(None)
    }
}

/// What one chat's run journal says it spent, and what spent it (gh#151).
///
/// The model rides along because the journal is the only place it is stated:
/// [`DispatchSpec::model`] is `None` on most attempts (the route named no
/// override, so the harness default ran), and a per-model breakdown whose
/// biggest row is "unknown" is not a breakdown. What the harness announced in
/// its `SessionStarted` is the model that actually ran.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunTokens {
    pub usage: comet_proto::TokenUsage,
    pub model: Option<String>,
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

    #[test]
    fn the_runtime_picker_offers_one_canonical_name_per_harness() {
        use std::collections::HashSet;
        // Every picker option is a valid override, and no two options map to
        // the same harness — offering both `claude-code` and `claude` would be
        // the same thing twice.
        let options = runtime_options();
        let harnesses: HashSet<HarnessId> = options
            .iter()
            .map(|o| harness_for_runtime(&o.name).expect("picker options must resolve"))
            .collect();
        assert_eq!(harnesses.len(), options.len());
        for (name, label) in options.iter().map(|o| (o.name.as_str(), o.label.as_str())) {
            assert_eq!(runtime_name(harness_for_runtime(name).unwrap()), name);
            assert!(!label.is_empty(), "{name} needs a picker label");
        }
    }

    #[test]
    fn runtime_name_round_trips_through_harness_for_runtime() {
        use HarnessId::*;
        for id in [ClaudeCode, Codex, Cursor, Opencode, Mock] {
            assert_eq!(harness_for_runtime(runtime_name(id)), Some(id));
        }
        // The canonical names are exactly the kebab-case harness ids.
        assert_eq!(runtime_name(HarnessId::Opencode), "opencode");
    }

    fn spec(identifier: &str, title: &str) -> DispatchSpec {
        DispatchSpec {
            identifier: identifier.into(),
            title: title.into(),
            space_id: "s".into(),
            device_id: "d".into(),
            repo_path: "/tmp".into(),
            branch: "b".into(),
            base: "origin/HEAD".into(),
            worktree: true,
            harness: HarnessId::Mock,
            model: None,
            account: None,
            push_repo: None,
            git_author: None,
            prompt: String::new(),
        }
    }

    #[test]
    fn a_chat_is_named_for_its_issue_and_not_only_its_number() {
        // The shelf the operator read on 2026-08-08: gh#10, gh#25, gh#26,
        // gh#11, gh#13 — five rows and not one of them says what it is.
        assert_eq!(
            spec("gh#25", "D1 Prototype v1: the Today window (static)").chat_title(),
            "gh#25 · D1 Prototype v1: the Today window (static)"
        );
        // The identifier leads, because the pane elides from the right and it
        // is the half that has to survive.
        assert!(
            spec("LIN-145", "Anything at all")
                .chat_title()
                .starts_with("LIN-145")
        );
    }

    #[test]
    fn a_long_title_is_clipped_at_a_word_and_an_empty_one_is_not_appended() {
        let long = spec(
            "gh#1",
            "A title that simply keeps going and going well past \
                                 any width a sidebar row could ever hope to give it",
        )
        .chat_title();
        assert!(
            long.chars().count() <= "gh#1 · ".len() + CHAT_TITLE_MAX + 1,
            "{long}"
        );
        assert!(long.ends_with('…'), "{long}");
        assert!(!long.contains("  "), "clipped at a word: {long}");

        // No dangling separator when the tracker has no title for us.
        assert_eq!(spec("gh#7", "   ").chat_title(), "gh#7");
        assert_eq!(spec("gh#7", "").chat_title(), "gh#7");
    }
}
