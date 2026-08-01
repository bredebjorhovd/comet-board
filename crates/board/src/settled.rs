//! The settle decision: what the end of a run means for the attempt that owns
//! the chat (docs/BOARD.md §H4).
//!
//! Ported from herdr-board's `settled.rs` and the settle half of its `sync.rs`
//! — the *decision*, deliberately without the machinery that surrounded it.
//! herdr had to infer "the agent finished" from a screen-classified `idle` that
//! flapped mid-turn, so the decision came wrapped in a 60-second debounce clock
//! (its gh#18/gh#34: two samples a fraction of a second apart satisfied a
//! counter, and gh#543 was closed twenty minutes before its agent stopped).
//! comet's run journal records a run ending as a durable event — the engine
//! writes `Done` and only then flips the session row — so the clock is gone
//! entirely: **the turn ended, now check the checkout.**
//!
//! What survives the port unchanged is the evidence hierarchy:
//!
//! - **A pull request is the agent's own statement that it is finished.** It
//!   cannot exist unless something ran, so there is nothing to second-guess:
//!   it closes the attempt on the spot, whatever the run's own exit said.
//! - **Commits are evidence, not a statement.** `agent-conventions.md` tells
//!   dispatched agents to commit even when they are not opening a PR, so the
//!   artifact is routinely there long before the work is done. They close the
//!   attempt only when the run genuinely ended — and never an `Errored` one,
//!   because an errored run died mid-task, and settling it as `finished` would
//!   tell the operator the opposite of what happened.
//! - **An `Errored` run stays live.** The chat is intact with its full
//!   context; retry-or-cancel is the operator's call, and a retried run is the
//!   *same attempt* — nobody dispatched anything, so nothing new is recorded.
//!
//! And so does the inverse (herdr gh#34): a settle is not the last time the
//! board looks. A closed attempt whose chat starts working again was closed
//! *wrongly*, and is re-opened rather than re-dispatched — see
//! [`crate::sync::SyncEngine`]'s rewatch. herdr needed a screen-moved check to
//! keep a stale spinner from flapping every finished row; comet's `Working` is
//! written by the engine that runs the agent and staleness-gated on the way in
//! ([`crate::runtime::agent_status`]), so the status alone is trustworthy.

use crate::model::AgentStatus;
use crate::runtime::RunEnd;

/// The artifact a settle closes on. Ranked: a pull request outranks commits,
/// and the writeback comment says which one the verdict rested on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    /// An open pull request on the attempt's branch — the agent's own
    /// statement of done.
    PullRequest,
    /// Commits past the attempt's base — the local equivalent of "explicit
    /// done detection", and the weaker of the two.
    Commits,
}

impl Evidence {
    /// How the log names it: "settled on PR" / "settled on commits".
    pub fn as_str(self) -> &'static str {
        match self {
            Evidence::PullRequest => "PR",
            Evidence::Commits => "commits",
        }
    }
}

/// What the end of a run means for the attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Close the attempt as done, on this evidence.
    Finished(Evidence),
    /// The attempt stays live. The variant says why, for the log.
    StayLive(Why),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// The run errored. The chat holds its full context, so the retry the
    /// operator (or a re-queued prompt) makes continues *this* attempt; ending
    /// it here would turn every transient harness error into a failed attempt.
    Errored,
    /// The run ended cleanly but left nothing to settle on. The agent may be
    /// mid-conversation — between turns is not finished.
    NoArtifacts,
}

/// The decision, pure: given how the last run ended and what the checkout
/// holds, is this attempt over?
///
/// `has_commits` is a closure because the commit check shells out to git and
/// the answer is only needed when nothing stronger decides first — a pull
/// request must short-circuit it, and an errored run must never reach it.
///
/// Pure so the hierarchy is testable without a chat, a checkout or a GitHub —
/// the caller owns the artifact lookups, this owns the ranking.
pub fn decide(end: RunEnd, pr_open: bool, has_commits: impl FnOnce() -> bool) -> Verdict {
    // A PR is proof, whatever the run's own exit said: a harness that crashed
    // moments after `gh pr create` still finished the work. This is also why
    // an errored run is checked at all rather than skipped outright.
    if pr_open {
        return Verdict::Finished(Evidence::PullRequest);
    }
    if end == RunEnd::Errored {
        return Verdict::StayLive(Why::Errored);
    }
    // Completed and Interrupted alike: an agent that did work and was then
    // stopped settles on its commits — that is a finished attempt, not a
    // failed one (herdr learned this from panes quit mid-session).
    if has_commits() {
        return Verdict::Finished(Evidence::Commits);
    }
    Verdict::StayLive(Why::NoArtifacts)
}

/// Should a *closed* attempt go back to work? (herdr gh#34, kept per §H4.)
///
/// `redispatched` refuses on a task that already has a live attempt — that
/// attempt is the current one, and the partial unique index would refuse the
/// reopen anyway. `upstream_final` / `local_done` refuse because a closed or
/// deleted issue, or a `mark done`, is somebody deciding the task is over; an
/// agent still typing does not overrule them.
pub fn should_reopen(
    status: AgentStatus,
    upstream_final: bool,
    local_done: bool,
    redispatched: bool,
) -> bool {
    // Only `Working` reopens. comet's Working means a run is executing right
    // now — written by the engine, staleness-gated by `agent_status` — so
    // unlike herdr's screen-sampled `working` there is no flap to guard
    // against and no screen-moved check to make.
    status == AgentStatus::Working && !upstream_final && !local_done && !redispatched
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- the hierarchy ---------------------------------------------------

    /// The agent's own statement outranks everything, including how the run
    /// exited: the PR exists, so the work is reviewable, so the attempt is
    /// finished. No clock, no second sample — the whole of §H4's headline.
    #[test]
    fn a_pull_request_closes_the_attempt_whatever_the_run_said() {
        for end in [RunEnd::Completed, RunEnd::Interrupted, RunEnd::Errored] {
            assert_eq!(
                decide(end, true, || panic!(
                    "a PR must short-circuit the commit check"
                )),
                Verdict::Finished(Evidence::PullRequest),
                "{end:?}"
            );
        }
    }

    #[test]
    fn commits_close_a_run_that_genuinely_ended() {
        assert_eq!(
            decide(RunEnd::Completed, false, || true),
            Verdict::Finished(Evidence::Commits)
        );
    }

    /// An agent that did work and was then stopped is a finished attempt, not
    /// a failed start — herdr settled quit panes on their commits, and the
    /// operator interrupting a run is the same fact here.
    #[test]
    fn an_interrupted_run_with_commits_still_settles() {
        assert_eq!(
            decide(RunEnd::Interrupted, false, || true),
            Verdict::Finished(Evidence::Commits)
        );
    }

    /// The `reopened` half of §H4's contract starts here: an errored run is
    /// not closed, so the retry that follows lands on the same attempt row.
    /// Commits do not change that — the agent committed and *then* died, which
    /// is exactly the case where "finished" would be the opposite of true.
    #[test]
    fn an_errored_run_stays_live_even_with_commits() {
        assert_eq!(
            decide(RunEnd::Errored, false, || panic!(
                "an errored run must never reach the commit check"
            )),
            Verdict::StayLive(Why::Errored)
        );
    }

    /// Between turns is not finished: a run can end cleanly with nothing to
    /// show because the agent is mid-conversation. The attempt stays live and
    /// the row renders a dim idle marker, exactly as before H4.
    #[test]
    fn a_clean_end_with_no_artifacts_settles_nothing() {
        assert_eq!(
            decide(RunEnd::Completed, false, || false),
            Verdict::StayLive(Why::NoArtifacts)
        );
    }

    // ---- reopening -------------------------------------------------------

    #[test]
    fn a_working_chat_reopens_its_settled_attempt() {
        assert!(should_reopen(AgentStatus::Working, false, false, false));
    }

    /// Everything that is not a run executing right now leaves the settle
    /// standing — in particular `Idle`, which is what every finished chat
    /// reads forever, and `Unknown`, which is a crashed engine, not work.
    #[test]
    fn only_working_reopens() {
        for s in [
            AgentStatus::Idle,
            AgentStatus::Done,
            AgentStatus::Blocked,
            AgentStatus::Unknown,
            AgentStatus::Missing,
        ] {
            assert!(!should_reopen(s, false, false, false), "{s:?}");
        }
    }

    /// A closed or deleted issue, a `mark done`, or a re-dispatch each name
    /// somebody whose decision outranks an agent still typing.
    #[test]
    fn reopening_is_refused_when_somebody_already_decided() {
        assert!(!should_reopen(AgentStatus::Working, true, false, false));
        assert!(!should_reopen(AgentStatus::Working, false, true, false));
        assert!(!should_reopen(AgentStatus::Working, false, false, true));
    }
}
