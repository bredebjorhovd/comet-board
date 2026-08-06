//! Deliver pull-request review comments back into the chat that wrote the PR
//! (docs/BOARD.md §H5; herdr-board gh#13).
//!
//! An orchestrator reviews a task and writes its feedback on the pull request.
//! The agent that wrote the PR never sees it: it is sitting in a live chat with
//! the entire task in context, and the review is a notification it has no way
//! to receive. A task in `review` keeps its chat — nothing disposes of a chat
//! before its task reaches `done` — so the author is still there, and delivery
//! is the same [`Runtime::prompt`] every dispatch brief uses. The missing piece
//! is only to notice the comment and decide it is worth delivering.
//!
//! ## The loop, and what closes it here
//!
//! The orchestrator and the agent act as the same GitHub identity — both are
//! the operator's token — so "skip my own comments" has nothing to key on. A
//! naive implementation delivers a comment, the agent replies on the PR, the
//! board sees a new comment and delivers it back, forever.
//!
//! herdr-board closed that loop with three mechanisms. Only the first survives
//! the port, and the other two are dropped *on purpose* (docs/BOARD.md §H5):
//!
//! 1. **A per-PR watermark, per endpoint**, of the last comment id consumed.
//!    An id below the watermark can never come back, so nothing is delivered
//!    twice. Kept — it is what makes every other property converge.
//! 2. ~~Deliver only while the agent is idle.~~ That rule existed because
//!    typing into a busy terminal was unsafe and unverifiable. `prompt` is a
//!    durable command-ledger entry — a steer into a live run, a send otherwise
//!    — so a busy chat is a fine delivery target, and the ledger's supersede
//!    rules collapse a pileup of deliveries instead of interleaving them.
//! 3. ~~A latch on the wake.~~ The latch existed to *attribute* the agent's
//!    own PR reply so it was consumed rather than delivered back — and its
//!    honest cost was that a human comment landing inside the wake window was
//!    swallowed unread. With no author to key on, attribution is only ever a
//!    time-window heuristic, and comet takes the opposite trade: nothing is
//!    swallowed, and an agent's own reply is relayed back into its chat
//!    **once**. The chat holds the whole conversation, so the agent recognises
//!    its own words — and [`compose`] says so explicitly, so recognising them
//!    requires no cleverness. The relay converges where the wake loop did not:
//!    the watermark consumes the reply in the same cycle that relays it, so
//!    the chain only continues if the agent writes on the PR again.
//!
//! What else survives unchanged: the `updated_at` gate (the PR list already
//! says whether anything happened, so the steady state costs no comment
//! fetches), the first-sight floor (never deliver a PR's back catalogue), the
//! actionability filter (the board's own writeback comments, empty approvals),
//! and the author check — the chat must still exist and its cwd must still be
//! the attempt's checkout, because delivering somebody else's review into an
//! unrelated session is worse than not delivering.

use crate::db::Db;
use crate::model::{Attempt, BoardState, Task};
use crate::runtime::Runtime;
use crate::sources::github::{Feedback, FeedbackKind, Github, PullRequest, Rest};
use crate::sync::SyncEngine;
use anyhow::Result;
use serde::{Deserialize, Serialize};

/// What the board has already delivered for one pull request.
///
/// Stored as JSON in `meta` under [`crate::sync::meta::reviews_for`]. A row of
/// its own would be a schema change for a fact that is pure bookkeeping and
/// worthless once the chat is gone. (herdr-board's version carried a wake
/// latch — `woke_at`/`saw_working`; those fields are gone with the latch, and
/// serde ignores them in state written before the port.)
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivered {
    /// Highest id consumed, per endpoint. The three sequences are unrelated, so
    /// one watermark cannot cover them.
    #[serde(default)]
    pub issue: i64,
    #[serde(default)]
    pub inline: i64,
    #[serde(default)]
    pub review: i64,
    /// The pull request's `updated_at` these watermarks were computed against.
    /// Unchanged means nothing has happened on the PR since, and none of the
    /// three comment endpoints is worth asking — which is the whole poll-cost
    /// story: one call per open PR per cycle would be wasteful, and the PR list
    /// already reports this.
    #[serde(default)]
    pub updated_at: String,
    /// Whether a missing chat has already been logged, so a task whose chat the
    /// operator archived does not write the same line every 30 seconds.
    #[serde(default)]
    pub noted_gone: bool,
}

impl Delivered {
    fn watermark(&self, kind: FeedbackKind) -> i64 {
        match kind {
            FeedbackKind::Issue => self.issue,
            FeedbackKind::Inline => self.inline,
            FeedbackKind::Review => self.review,
        }
    }

    fn record(&mut self, f: &Feedback) {
        let slot = match f.kind {
            FeedbackKind::Issue => &mut self.issue,
            FeedbackKind::Inline => &mut self.inline,
            FeedbackKind::Review => &mut self.review,
        };
        *slot = (*slot).max(f.id);
    }

    /// Have we never looked at this pull request before?
    fn first_sight(&self) -> bool {
        self.updated_at.is_empty()
    }
}

/// What the pass decided about one pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Nothing has happened on the pull request since the last look. Costs no
    /// GitHub call at all.
    Unchanged,
    /// Nothing above the watermark is worth telling anyone about.
    NothingNew,
    /// Queue these into the authoring chat.
    Deliver(Vec<Feedback>),
}

/// Is this worth delivering into an agent's chat?
///
/// The board's own writeback comments are not: it posts a dispatch line and an
/// outcome line on every task it touches, and on a pull-request row those land
/// in the same conversation a reviewer writes in. They are the one thing
/// author-blind filtering *can* recognise, because the board wrote them.
pub fn is_actionable(f: &Feedback) -> bool {
    if is_the_boards_own(&f.body) {
        return false;
    }
    // A verdict of `changes requested` is actionable with or without prose. An
    // approval without prose is not: it says the agent has nothing left to do,
    // and interrupting it to hear so is the opposite of useful.
    f.requests_changes() || !f.body.trim().is_empty()
}

pub(crate) fn is_the_boards_own(body: &str) -> bool {
    let b = body.trim_start();
    b.starts_with("comet-board:") || b.starts_with("Dispatched to comet")
}

/// Decide what to do about one pull request, asking GitHub only if the decision
/// needs it.
///
/// Split from the effects so the part that has to converge — the watermarks —
/// is testable without an engine or a network. `state` is mutated in place;
/// the caller persists it unless the delivery itself failed, in which case
/// dropping it is what makes the next cycle retry.
///
/// `floor` is the authoring attempt's end (or start, if it is somehow still
/// live). It applies only on first sight, and only to stop the board delivering
/// a pull request's entire back catalogue the first time it looks at one:
/// nothing written before the agent finished can be a review of its work.
pub fn plan_delivery(
    state: &mut Delivered,
    pr_updated_at: &str,
    floor: &str,
    fetch: impl FnOnce() -> Result<Vec<Feedback>>,
) -> Result<Decision> {
    // The poll-cost gate: the pull request list already told us nothing has
    // happened, so none of the three comment endpoints is asked. An absent
    // timestamp is not a match — it would gate this off permanently.
    if !pr_updated_at.is_empty() && state.updated_at == pr_updated_at {
        return Ok(Decision::Unchanged);
    }

    let first_sight = state.first_sight();
    let feedback = fetch()?;
    let fresh: Vec<Feedback> = feedback
        .iter()
        .filter(|f| f.id > state.watermark(f.kind))
        .filter(|f| is_actionable(f))
        .filter(|f| !first_sight || f.created_at.as_str() > floor)
        .cloned()
        .collect();

    // Consume everything seen, delivered or not. An id below the watermark can
    // never come back, which is what stops the same comment arriving twice —
    // and what makes the own-reply relay a single bounce rather than a loop.
    for f in &feedback {
        state.record(f);
    }
    state.updated_at = pr_updated_at.to_string();

    if fresh.is_empty() {
        return Ok(Decision::NothingNew);
    }
    Ok(Decision::Deliver(fresh))
}

/// The pull request a task's PR fields point at, among the ones just polled.
///
/// Matched on the URL rather than the number: a number is only unique within a
/// repo, and the polled list spans every repo the board watches.
pub fn pull_request_for<'a>(task: &Task, pulls: &'a [PullRequest]) -> Option<&'a PullRequest> {
    let url = task.pr_url.as_deref()?;
    pulls.iter().find(|p| p.url == url)
}

/// The attempt that wrote this pull request.
///
/// The branch is the proof, and the only one there is: it is the link dispatch
/// creates, and a task can have several attempts with several chats. Without a
/// branch match we do not know which session produced the PR — and delivering
/// somebody else's review into an unrelated session is worse than not
/// delivering.
pub fn authoring_attempt<'a>(task: &'a Task, pr: &PullRequest) -> Option<&'a Attempt> {
    task.attempts
        .iter()
        .rev()
        .find(|a| a.pane_id.is_some() && a.branch.as_deref() == Some(pr.head_ref.as_str()))
}

/// Is this chat still where the agent wrote the pull request?
///
/// Chat ids are not reused, so `chat_alive` answering yes is most of it. The
/// rest is the chat row's cwd: an operator can re-point a chat at another
/// checkout, and pasting a review into a session that has moved on to other
/// work delivers it to the wrong author. `starts_with`, not equality — a chat
/// whose cwd is a subdirectory of its own worktree is still in its own
/// worktree. A chat with no recorded cwd is trusted on its id alone, which is
/// all there is and enough to not deliver blind.
pub fn still_the_authors_checkout(chat_cwd: Option<&str>, attempt: &Attempt) -> bool {
    match (chat_cwd, attempt.worktree.as_deref()) {
        (Some(cwd), Some(worktree)) => {
            std::path::Path::new(cwd).starts_with(std::path::Path::new(worktree))
        }
        _ => true,
    }
}

/// The message an agent's chat is queued with.
///
/// Enough to act on without a lookup: the body, the pull request URL, and for
/// an inline comment the file and line it hangs off. The tail states the one
/// consequence of author-blind delivery — a comment the agent itself wrote may
/// come back once — so recognising the relay takes no cleverness.
pub fn compose(task: &Task, pr: &PullRequest, items: &[Feedback]) -> String {
    let mut s = format!(
        "comet-board: your pull request has been reviewed.\n\n  \
         {} · {}\n  {}\n",
        task.identifier, task.title, pr.url
    );
    for f in items {
        s.push('\n');
        match (f.kind, f.state.as_deref()) {
            (FeedbackKind::Review, Some(state)) => {
                s.push_str(&format!("[review · {}]\n", state.replace('_', " ")));
            }
            (FeedbackKind::Inline, _) => {
                let where_ = match (f.path.as_deref(), f.line) {
                    (Some(p), Some(l)) => format!("{p}:{l}"),
                    (Some(p), None) => p.to_string(),
                    // An inline comment whose anchor GitHub no longer reports.
                    _ => "(inline)".to_string(),
                };
                s.push_str(&format!("[{where_}]\n"));
            }
            _ => {}
        }
        let body = f.body.trim();
        if body.is_empty() {
            // Only reachable for `changes requested` with no prose, which is
            // still the loudest thing on the list.
            s.push_str("(no comment body — see the pull request)\n");
        } else {
            s.push_str(body);
            s.push('\n');
        }
    }
    s.push_str(
        "\nThe pull request is still open and you are still in the checkout you wrote it \
         in. Address the feedback there, commit, and push to the same branch — do not open \
         a second pull request. If you disagree with a point, say so on the pull request \
         rather than silently skipping it. The board cannot tell comment authors apart, so \
         a comment you write on the pull request may be relayed back to you once — if a \
         comment above is your own, it needs nothing from you.\n",
    );
    s
}

/// Load a task's delivery state. A missing or unreadable value starts over,
/// which costs one round of re-consumption and never a wrong delivery.
fn load(db: &Db, task_id: &str) -> Delivered {
    db.meta_get(&crate::sync::meta::reviews_for(task_id))
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

fn store(db: &Db, task_id: &str, state: &Delivered) -> Result<()> {
    db.meta_set(
        &crate::sync::meta::reviews_for(task_id),
        &serde_json::to_string(state)?,
    )?;
    Ok(())
}

impl SyncEngine {
    /// Queue new review comments into the chats whose pull requests they are
    /// about.
    ///
    /// Runs after a sync cycle, because `review` is the state that keeps a
    /// chat and it is only correct once that cycle's reconciliation has
    /// landed. Takes the pull requests the cycle already polled rather than
    /// asking GitHub again per task.
    pub fn deliver_reviews(&self, runtime: &dyn Runtime, pulls: &[PullRequest]) {
        if !self.cfg.github.deliver_reviews {
            return;
        }
        let Some(gh) = &self.github else {
            return;
        };
        if pulls.is_empty() {
            return;
        }
        let tasks = match self.db.load_tasks() {
            Ok(t) => t,
            Err(e) => {
                self.log
                    .error(format!("reading tasks for review delivery: {e}"));
                return;
            }
        };
        for task in tasks {
            if let Err(e) = self.deliver_review_for(gh, runtime, pulls, &task) {
                self.log
                    .warn(format!("delivering review for {}: {e}", task.identifier));
            }
        }
    }

    fn deliver_review_for(
        &self,
        gh: &Github<Box<dyn Rest>>,
        runtime: &dyn Runtime,
        pulls: &[PullRequest],
        task: &Task,
    ) -> Result<()> {
        // `review` is the whole precondition: finished work with an open pull
        // request, whose chat nothing has disposed of yet.
        if task.state != BoardState::Review || !task.pr_open {
            return Ok(());
        }
        let Some(pr) = pull_request_for(task, pulls) else {
            return Ok(());
        };
        let Some(attempt) = authoring_attempt(task, pr) else {
            return Ok(());
        };
        let Some(chat_id) = attempt.pane_id.as_deref() else {
            return Ok(());
        };
        let mut state = load(&self.db, &task.id);

        // A task whose chat is gone is skipped quietly. There is nothing to
        // deliver to and nothing to do about it — re-dispatching would be a
        // second agent on work that is already written. A runtime error here
        // propagates instead: not knowing is not the same as gone, and the
        // next cycle asks again.
        let gone = !runtime.chat_alive(chat_id)?
            || !still_the_authors_checkout(runtime.chat_cwd(chat_id)?.as_deref(), attempt);
        if gone {
            if !state.noted_gone {
                self.log.info(format!(
                    "{}: chat {chat_id} no longer holds the agent that wrote {} — \
                     review comments will not be delivered",
                    task.identifier, pr.url
                ));
                state.noted_gone = true;
                store(&self.db, &task.id, &state)?;
            }
            return Ok(());
        }
        state.noted_gone = false;

        let floor = attempt
            .ended_at
            .clone()
            .unwrap_or_else(|| attempt.started_at.clone());

        let decision = plan_delivery(&mut state, &pr.updated_at, &floor, || {
            gh.pr_feedback(&pr.repo, pr.number)
        })?;

        if let Decision::Deliver(items) = &decision {
            let text = compose(task, pr, items);
            if let Err(e) = runtime.prompt(chat_id, &text) {
                // Nothing arrived, so nothing was consumed: dropping the state
                // here is what makes the next cycle try again.
                self.log.warn(format!(
                    "{}: could not queue {} review comment(s) into chat {chat_id}: {e}",
                    task.identifier,
                    items.len()
                ));
                return Ok(());
            }
            self.log.info(format!(
                "{}: delivered {} review comment(s) on {} into chat {chat_id}",
                task.identifier,
                items.len(),
                pr.url
            ));
        }
        store(&self.db, &task.id, &state)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentStatus, Outcome, Source, UpstreamState};
    use crate::runtime::{DispatchHandle, DispatchSpec};
    use comet_proto::Session;

    fn feedback(kind: FeedbackKind, id: i64, body: &str, created_at: &str) -> Feedback {
        Feedback {
            kind,
            id,
            body: body.into(),
            url: format!("https://github.com/o/r/pull/14#c{id}"),
            created_at: created_at.into(),
            author: "bredebjorhovd".into(),
            path: None,
            line: None,
            state: None,
        }
    }

    fn verdict(id: i64, state: &str, body: &str, created_at: &str) -> Feedback {
        Feedback {
            state: Some(state.into()),
            ..feedback(FeedbackKind::Review, id, body, created_at)
        }
    }

    fn inline(id: i64, path: &str, line: i64, body: &str, created_at: &str) -> Feedback {
        Feedback {
            path: Some(path.into()),
            line: Some(line),
            ..feedback(FeedbackKind::Inline, id, body, created_at)
        }
    }

    const FLOOR: &str = "2026-07-28T10:00:00Z";

    fn plan(state: &mut Delivered, updated_at: &str, items: Vec<Feedback>) -> Decision {
        plan_delivery(state, updated_at, FLOOR, || Ok(items)).unwrap()
    }

    /// The steady state, and the one that has to be free: the pull request has
    /// not moved, so none of the three comment endpoints is asked at all.
    #[test]
    fn an_unchanged_pull_request_costs_no_api_call() {
        let mut state = Delivered {
            updated_at: "2026-07-28T11:00:00Z".into(),
            ..Default::default()
        };
        let decision = plan_delivery(&mut state, "2026-07-28T11:00:00Z", FLOOR, || {
            panic!("nothing moved on the PR; there is nothing to ask about")
        })
        .unwrap();
        assert_eq!(decision, Decision::Unchanged);
    }

    #[test]
    fn a_changes_requested_review_is_delivered_and_watermarked() {
        let mut state = Delivered {
            updated_at: "2026-07-28T11:00:00Z".into(),
            ..Default::default()
        };
        let d = plan(
            &mut state,
            "2026-07-28T11:30:00Z",
            vec![verdict(
                900,
                "changes_requested",
                "Split the watermark per endpoint.",
                "2026-07-28T11:30:00Z",
            )],
        );
        let Decision::Deliver(items) = d else {
            panic!("expected a delivery, got {d:?}");
        };
        assert_eq!(items.len(), 1);
        assert!(items[0].requests_changes());
        assert_eq!(state.review, 900, "the watermark moved past it");
    }

    /// The loop the watermark exists to break. Without herdr-board's wake
    /// latch there is no attribution, so the agent's own reply IS relayed back
    /// — once. The watermark consumes it in the same cycle, so the chain ends
    /// unless the agent writes on the pull request again. This test pins the
    /// single bounce and the convergence.
    #[test]
    fn an_agents_own_reply_is_relayed_once_and_never_twice() {
        let mut state = Delivered {
            updated_at: "2026-07-28T11:00:00Z".into(),
            ..Default::default()
        };
        // The orchestrator asks for changes.
        let d = plan(
            &mut state,
            "2026-07-28T11:30:00Z",
            vec![verdict(
                900,
                "changes_requested",
                "Fix the gate.",
                "2026-07-28T11:30:00Z",
            )],
        );
        assert!(matches!(d, Decision::Deliver(_)));

        // The agent replies on the pull request. Same GitHub identity as the
        // orchestrator, so nothing about the comment says who wrote it — it is
        // relayed back into the chat, which is the accepted cost of dropping
        // the latch.
        let reply = feedback(
            FeedbackKind::Issue,
            41,
            "Fixed in abc123.",
            "2026-07-28T11:40:00Z",
        );
        let d = plan(&mut state, "2026-07-28T11:40:00Z", vec![reply.clone()]);
        assert!(matches!(&d, Decision::Deliver(items) if items == &vec![reply]));
        assert_eq!(state.issue, 41, "consumed in the same cycle it was relayed");

        // The relay provokes no new PR comment, so the chain is over: the PR
        // has not moved, and even when it moves again the reply cannot return.
        let d = plan_delivery(&mut state, "2026-07-28T11:40:00Z", FLOOR, || {
            panic!("unchanged PR; not asked")
        })
        .unwrap();
        assert_eq!(d, Decision::Unchanged);
        let d = plan(
            &mut state,
            "2026-07-28T11:50:00Z",
            vec![feedback(
                FeedbackKind::Issue,
                41,
                "Fixed in abc123.",
                "2026-07-28T11:40:00Z",
            )],
        );
        assert_eq!(d, Decision::NothingNew, "below the watermark, gone forever");

        // And the board is still listening: the next real review lands.
        let d = plan(
            &mut state,
            "2026-07-28T12:00:00Z",
            vec![feedback(
                FeedbackKind::Issue,
                42,
                "Still wrong on the retry path.",
                "2026-07-28T12:00:00Z",
            )],
        );
        assert!(matches!(d, Decision::Deliver(items) if items.len() == 1));
    }

    /// First sight must not hand an agent the pull request's whole back
    /// catalogue — including its own PR-opening chatter.
    #[test]
    fn the_first_look_at_a_pull_request_delivers_nothing_older_than_the_work() {
        let mut state = Delivered::default();
        assert!(state.first_sight());
        let d = plan(
            &mut state,
            "2026-07-28T12:00:00Z",
            vec![
                // Written by the agent while it still held the task.
                feedback(
                    FeedbackKind::Issue,
                    10,
                    "Opened the PR.",
                    "2026-07-28T09:30:00Z",
                ),
                // Written after it finished: a genuine review.
                verdict(
                    900,
                    "changes_requested",
                    "Not quite.",
                    "2026-07-28T11:00:00Z",
                ),
            ],
        );
        let Decision::Deliver(items) = d else {
            panic!("expected the review to be delivered, got {d:?}");
        };
        assert_eq!(items.len(), 1, "only what came after the work");
        assert!(items[0].requests_changes());
        assert_eq!(state.issue, 10, "the older one is consumed, not delivered");
    }

    #[test]
    fn the_boards_own_writeback_comments_are_not_review_feedback() {
        // On a pull-request row these land in the same conversation a reviewer
        // writes in, and interrupting the agent with its own dispatch line is
        // noise.
        assert!(!is_actionable(&feedback(
            FeedbackKind::Issue,
            1,
            "Dispatched to comet · claude-code · space:offhand · attempt 1",
            "t"
        )));
        assert!(!is_actionable(&feedback(
            FeedbackKind::Issue,
            2,
            "comet-board: attempt finished · https://github.com/o/r/pull/14",
            "t"
        )));
        assert!(is_actionable(&feedback(
            FeedbackKind::Issue,
            3,
            "The retry path is still wrong.",
            "t"
        )));
    }

    #[test]
    fn an_approval_with_nothing_to_say_interrupts_nobody() {
        assert!(!is_actionable(&verdict(1, "approved", "", "t")));
        // With prose it is worth hearing.
        assert!(is_actionable(&verdict(
            2,
            "approved",
            "Nice — rename `x` first.",
            "t"
        )));
        // And a verdict of changes requested is actionable with or without it.
        assert!(is_actionable(&verdict(3, "changes_requested", "", "t")));
        // An empty comment is nothing at all.
        assert!(!is_actionable(&feedback(
            FeedbackKind::Issue,
            4,
            "   ",
            "t"
        )));
    }

    #[test]
    fn the_message_says_where_an_inline_comment_points() {
        let task = task_row();
        let pr = pull("board/gh-13");
        let text = compose(
            &task,
            &pr,
            &[
                verdict(900, "changes_requested", "Two things.", "t"),
                inline(41, "src/review.rs", 88, "This anchors to a line.", "t"),
            ],
        );
        assert!(text.contains("https://github.com/o/r/pull/14"), "{text}");
        assert!(text.contains("gh#13"), "{text}");
        assert!(text.contains("[review · changes requested]"), "{text}");
        assert!(text.contains("[src/review.rs:88]"), "{text}");
        assert!(text.contains("This anchors to a line."), "{text}");
        // Enough to act on without a lookup, and told where to put the fix.
        assert!(text.contains("push to the same branch"), "{text}");
        // The one consequence of author-blind delivery is stated, not sprung.
        assert!(text.contains("relayed back to you once"), "{text}");
    }

    // ---- chat verification ----------------------------------------------

    fn task_row() -> Task {
        Task {
            id: "gh:o/r#13".into(),
            source: Source::Github,
            source_id: "n".into(),
            identifier: "gh#13".into(),
            title: "Deliver PR review comments back into the agent's chat".into(),
            body: None,
            url: "https://github.com/o/r/issues/13".into(),
            labels: vec![],
            state: BoardState::Review,
            source_state: None,
            linear_team: None,
            linear_project: None,
            upstream: UpstreamState::Unstarted,
            local_done: false,
            pr_url: Some("https://github.com/o/r/pull/14".into()),
            pr_number: Some(14),
            pr_open: true,
            pr_merged: false,
            pr_mergeable: None,
            updated_at: "t".into(),
            synced_at: "t".into(),
            attempts: vec![],
        }
    }

    fn pull(head_ref: &str) -> PullRequest {
        PullRequest {
            repo: "o/r".into(),
            number: 14,
            title: "Deliver review comments".into(),
            body: None,
            url: "https://github.com/o/r/pull/14".into(),
            head_ref: head_ref.into(),
            open: true,
            merged: false,
            draft: false,
            updated_at: "2026-07-28T11:30:00Z".into(),
        }
    }

    fn attempt(branch: &str, chat: &str, worktree: Option<&str>) -> Attempt {
        Attempt {
            id: 1,
            task_id: "gh:o/r#13".into(),
            pane_id: Some(chat.into()),
            workspace: "offhand".into(),
            runtime: "claude-code".into(),
            account: None,
            worktree: worktree.map(str::to_string),
            branch: Some(branch.into()),
            started_at: "2026-07-28T09:00:00Z".into(),
            ended_at: Some("2026-07-28T10:00:00Z".into()),
            outcome: Some(Outcome::Done),
            missing_ticks: 0,
            agent_status: Some(AgentStatus::Idle),
            dispatched_by: None,
            dispatched_by_pane: None,
            base_sha: None,
            saw_working: true,
            settled_at: None,
            reopened: 0,
            screen_print: None,
            screen_at: None,
            nudges: 0,
            nudged_at: None,
            blocked_count: 0,
            overrun_warned_at: None,
            repo_path: Some("/repo/r".into()),
            collectable_at: None,
            collected_at: None,
            dispatched_by_device: None,
            dispatched_by_user: None,
        }
    }

    /// The branch is the only proof of which session wrote the PR. A task can
    /// hold several attempts in several chats — the dispatcher can override
    /// the branch — and a review must not go to whichever is newest.
    #[test]
    fn only_the_attempt_whose_branch_matches_owns_the_pull_request() {
        let mut task = task_row();
        task.attempts = vec![
            attempt("board/gh-13", "chat-1", None),
            Attempt {
                id: 2,
                ..attempt("spike/another-idea", "chat-9", None)
            },
        ];
        let pr = pull("board/gh-13");
        assert_eq!(
            authoring_attempt(&task, &pr).unwrap().pane_id.as_deref(),
            Some("chat-1")
        );
        // A pull request nothing on this task branched to belongs to nobody
        // here, and nobody here is told about it.
        assert!(authoring_attempt(&task, &pull("somebody/else")).is_none());
    }

    #[test]
    fn a_chat_that_moved_to_another_checkout_is_not_the_author() {
        let a = attempt("board/gh-13", "chat-1", Some("/wt/gh-13-1"));
        assert!(still_the_authors_checkout(Some("/wt/gh-13-1"), &a));
        // Deeper inside its own checkout is still its own checkout.
        assert!(still_the_authors_checkout(Some("/wt/gh-13-1/src"), &a));
        // Somebody else's session entirely.
        assert!(!still_the_authors_checkout(Some("/wt/lin-140-1"), &a));
        // No recorded cwd: the chat id is all there is, and it is enough.
        assert!(still_the_authors_checkout(None, &a));
    }

    #[test]
    fn a_pull_request_is_matched_to_its_task_by_url_not_number() {
        let task = task_row();
        let mut other = pull("board/gh-13");
        other.repo = "someone/else".into();
        other.url = "https://github.com/someone/else/pull/14".into();
        // Same number, different repo: not this task's pull request.
        assert!(pull_request_for(&task, std::slice::from_ref(&other)).is_none());
        assert_eq!(
            pull_request_for(&task, &[other, pull("board/gh-13")])
                .unwrap()
                .repo,
            "o/r"
        );
    }

    // ---- the pass, driven through the engine ----------------------------

    /// Answers like a healthy comet and records what the board queued. The
    /// chat's whereabouts are the knobs the tests turn.
    struct FakeRuntime {
        alive: bool,
        cwd: Option<String>,
        prompts: std::cell::RefCell<Vec<(String, String)>>,
        prompt_fails: bool,
    }

    impl FakeRuntime {
        fn holding(cwd: &str) -> FakeRuntime {
            FakeRuntime {
                alive: true,
                cwd: Some(cwd.into()),
                prompts: Default::default(),
                prompt_fails: false,
            }
        }

        fn gone() -> FakeRuntime {
            FakeRuntime {
                alive: false,
                cwd: None,
                prompts: Default::default(),
                prompt_fails: false,
            }
        }
    }

    impl Runtime for FakeRuntime {
        fn dispatch(&self, _spec: &DispatchSpec) -> Result<DispatchHandle> {
            panic!("review delivery never dispatches")
        }
        fn prompt(&self, chat_id: &str, text: &str) -> Result<()> {
            if self.prompt_fails {
                anyhow::bail!("the ledger is unreachable");
            }
            self.prompts
                .borrow_mut()
                .push((chat_id.to_string(), text.to_string()));
            Ok(())
        }
        fn cancel(&self, _chat_id: &str) -> Result<()> {
            Ok(())
        }
        fn session(&self, _chat_id: &str) -> Result<Option<Session>> {
            Ok(None)
        }
        fn chat_alive(&self, _chat_id: &str) -> Result<bool> {
            Ok(self.alive)
        }
        fn chat_cwd(&self, _chat_id: &str) -> Result<Option<String>> {
            Ok(self.cwd.clone())
        }
        fn last_run_end(&self, _chat_id: &str) -> Result<Option<crate::runtime::RunEnd>> {
            Ok(None)
        }
    }

    /// Shares one fixture between the engine, which owns its transport, and the
    /// test, which needs to read back what was asked.
    struct Shared(std::rc::Rc<crate::sources::github::FixtureRest>);

    impl Rest for Shared {
        fn get(&self, path: &str) -> Result<serde_json::Value> {
            self.0.get(path)
        }
        fn post(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
            self.0.post(path, body)
        }
        fn patch(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
            self.0.patch(path, body)
        }
        fn put(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
            self.0.put(path, body)
        }
    }

    /// An engine over an in-memory board and a recorded GitHub.
    fn engine(rest: std::rc::Rc<crate::sources::github::FixtureRest>) -> SyncEngine {
        let dir = std::env::temp_dir().join(format!("cb-review-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        SyncEngine {
            db: Db::open_in_memory().unwrap(),
            cfg: crate::config::RoutingConfig::default(),
            credentials: Default::default(),
            paths: crate::config::Paths {
                config_dir: dir.clone(),
                state_dir: dir,
            },
            log: std::sync::Arc::new(crate::log::Logger::new("", false)),
            linear: None,
            github: Some(Github::new(Box::new(Shared(rest)) as Box<dyn Rest>)),
            webhook: std::sync::Arc::new(crate::notify::HttpWebhook),
        }
    }

    /// A finished attempt on gh#13, with an open pull request — the row this
    /// whole module is about.
    fn seed_reviewed_task(e: &SyncEngine, branch: &str, chat: &str) {
        e.db.upsert_task(&crate::db::UpsertTask {
            id: "gh:o/r#13".into(),
            source: Source::Github,
            source_id: "n".into(),
            identifier: "gh#13".into(),
            title: "Deliver PR review comments".into(),
            body: None,
            url: "https://github.com/o/r/issues/13".into(),
            labels: vec![],
            source_state: Some("open".into()),
            linear_team: None,
            linear_project: None,
            upstream: UpstreamState::Unstarted,
            updated_at: crate::db::now(),
        })
        .unwrap();
        let a =
            e.db.insert_attempt(&crate::db::NewAttempt {
                task_id: "gh:o/r#13".into(),
                pane_id: None,
                workspace: "offhand".into(),
                runtime: "claude-code".into(),
                worktree: Some("/wt/gh-13-1".into()),
                branch: Some(branch.into()),
                dispatched_by: None,
                dispatched_by_pane: None,
                base_sha: None,
                account: None,
                repo_path: None,
                dispatched_by_device: None,
                dispatched_by_user: None,
            })
            .unwrap();
        e.db.set_attempt_pane(a, chat).unwrap();
        e.db.close_attempt(a, Outcome::Done).unwrap();
        e.db.set_pr(
            "gh:o/r#13",
            Some("https://github.com/o/r/pull/14"),
            Some(14),
            true,
        )
        .unwrap();
        e.rederive_all().unwrap();
        assert_eq!(
            e.db.get_task("gh:o/r#13").unwrap().unwrap().state,
            BoardState::Review
        );
    }

    fn state_of(e: &SyncEngine) -> Delivered {
        load(&e.db, "gh:o/r#13")
    }

    fn feedback_fixture() -> Vec<(String, serde_json::Value)> {
        vec![
            (
                // Written by the agent before the attempt closed — the seeded
                // attempt's `ended_at` is the test's own wall clock, so this
                // sits below the first-sight floor and is consumed unspoken.
                "/repos/o/r/issues/14/comments".into(),
                serde_json::json!([{ "id": 41, "body": "Opened the PR.",
                                     "created_at": "2026-07-28T09:30:00Z",
                                     "user": { "login": "b" } }]),
            ),
            ("/repos/o/r/pulls/14/comments".into(), serde_json::json!([])),
            (
                // A genuine review lands after the work is finished. "After"
                // has to outlive the test run's clock, hence the far date.
                "/repos/o/r/pulls/14/reviews".into(),
                serde_json::json!([{ "id": 900, "state": "CHANGES_REQUESTED",
                                     "body": "Split the watermark per endpoint.",
                                     "submitted_at": "2999-01-01T00:00:00Z",
                                     "user": { "login": "b" } }]),
            ),
        ]
    }

    /// The chat the agent was in is gone — archived, or deleted with its
    /// workspace. Nothing to deliver to, and nothing to do about it: a
    /// re-dispatch would be a second agent on work that is already written.
    #[test]
    fn a_task_whose_chat_is_gone_is_skipped_quietly() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(vec![]));
        let e = engine(rest.clone());
        seed_reviewed_task(&e, "board/gh-13", "chat-1");

        e.deliver_reviews(&FakeRuntime::gone(), &[pull("board/gh-13")]);

        assert!(
            rest.asked.borrow().is_empty(),
            "a chat that is gone costs no GitHub call: {:?}",
            rest.asked.borrow()
        );
        assert!(
            state_of(&e).noted_gone,
            "and it is said once, not every tick"
        );
    }

    /// A live chat whose cwd is somebody else's checkout is the same verdict
    /// as a gone chat: the author is not there to deliver to.
    #[test]
    fn a_chat_repointed_at_another_checkout_is_not_delivered_into() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(vec![]));
        let e = engine(rest.clone());
        seed_reviewed_task(&e, "board/gh-13", "chat-1");

        let runtime = FakeRuntime::holding("/wt/lin-140-1");
        e.deliver_reviews(&runtime, &[pull("board/gh-13")]);

        assert!(rest.asked.borrow().is_empty());
        assert!(runtime.prompts.borrow().is_empty());
        assert!(state_of(&e).noted_gone);
    }

    /// The candidate path, right up to the point where a delivery would
    /// happen: task selected, chat verified, all three endpoints asked,
    /// watermarks recorded. Nothing here is new enough to tell anyone about.
    #[test]
    fn a_verified_chat_with_nothing_new_records_the_watermark_and_stops() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(vec![
            (
                "/repos/o/r/issues/14/comments".into(),
                serde_json::json!([{ "id": 41, "body": "Opened the PR.",
                                     "created_at": "2026-07-28T09:30:00Z",
                                     "user": { "login": "b" } }]),
            ),
            ("/repos/o/r/pulls/14/comments".into(), serde_json::json!([])),
            ("/repos/o/r/pulls/14/reviews".into(), serde_json::json!([])),
        ]));
        let e = engine(rest.clone());
        seed_reviewed_task(&e, "board/gh-13", "chat-1");
        let runtime = FakeRuntime::holding("/wt/gh-13-1");

        e.deliver_reviews(&runtime, &[pull("board/gh-13")]);

        assert_eq!(rest.asked.borrow().len(), 3, "all three sources are read");
        let state = state_of(&e);
        assert_eq!(state.issue, 41, "consumed, so it can never come back");
        assert_eq!(state.updated_at, "2026-07-28T11:30:00Z");
        assert!(runtime.prompts.borrow().is_empty(), "nobody was told");

        // And a second cycle over an unchanged pull request asks nothing at all.
        e.deliver_reviews(&runtime, &[pull("board/gh-13")]);
        assert_eq!(rest.asked.borrow().len(), 3, "the updated_at gate held");
    }

    /// The whole point: a review lands, and the chat that wrote the pull
    /// request is queued with it.
    #[test]
    fn a_review_is_queued_into_the_authoring_chat() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(feedback_fixture()));
        let e = engine(rest.clone());
        seed_reviewed_task(&e, "board/gh-13", "chat-1");
        let runtime = FakeRuntime::holding("/wt/gh-13-1");

        e.deliver_reviews(&runtime, &[pull("board/gh-13")]);

        let prompts = runtime.prompts.borrow();
        assert_eq!(prompts.len(), 1);
        let (chat, text) = &prompts[0];
        assert_eq!(chat, "chat-1");
        assert!(text.contains("changes requested"), "{text}");
        assert!(text.contains("Split the watermark per endpoint."), "{text}");
        assert!(
            !text.contains("Opened the PR."),
            "the agent's own pre-finish chatter is below the floor: {text}"
        );
        drop(prompts);

        let state = state_of(&e);
        assert_eq!(state.review, 900);
        assert_eq!(state.issue, 41);
    }

    /// A delivery that never reached the ledger must not consume what it
    /// failed to deliver: the state is dropped, and the next cycle retries.
    #[test]
    fn a_failed_delivery_is_retried_rather_than_consumed() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(feedback_fixture()));
        let e = engine(rest.clone());
        seed_reviewed_task(&e, "board/gh-13", "chat-1");

        let mut runtime = FakeRuntime::holding("/wt/gh-13-1");
        runtime.prompt_fails = true;
        e.deliver_reviews(&runtime, &[pull("board/gh-13")]);
        assert_eq!(
            state_of(&e),
            Delivered::default(),
            "nothing was consumed by the failure"
        );

        runtime.prompt_fails = false;
        e.deliver_reviews(&runtime, &[pull("board/gh-13")]);
        assert_eq!(runtime.prompts.borrow().len(), 1, "the retry delivered");
        assert_eq!(state_of(&e).review, 900);
    }

    /// A pull request nobody dispatched has no author sitting in a chat. It is
    /// a `review` row like any other, and there is no one to tell.
    #[test]
    fn a_pull_request_row_the_board_never_dispatched_tells_nobody() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(vec![]));
        let e = engine(rest.clone());
        let pr = pull("someone/elses-branch");
        e.db.upsert_task(&pr.to_upsert()).unwrap();
        e.db.set_pr(&pr.task_id(), Some(&pr.url), Some(pr.number), true)
            .unwrap();
        e.rederive_all().unwrap();

        e.deliver_reviews(&FakeRuntime::holding("/wt/gh-13-1"), &[pr]);
        assert!(rest.asked.borrow().is_empty());
    }

    #[test]
    fn turning_delivery_off_stops_it_asking_anything() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(vec![]));
        let mut e = engine(rest.clone());
        e.cfg.github.deliver_reviews = false;
        seed_reviewed_task(&e, "board/gh-13", "chat-1");
        e.deliver_reviews(&FakeRuntime::holding("/wt/gh-13-1"), &[pull("board/gh-13")]);
        assert!(rest.asked.borrow().is_empty());
    }
}
