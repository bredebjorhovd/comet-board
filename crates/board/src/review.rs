//! Deliver pull-request review comments back into the chat that wrote the PR
//! (§review-delivery; herdr-board gh#13).
//!
//! A reviewer reads a task and writes its feedback on the pull request.
//! The agent that wrote the PR never sees it: it is sitting in a live chat with
//! the entire task in context, and the review is a notification it has no way
//! to receive. A task in `review` keeps its chat — nothing disposes of a chat
//! before its task reaches `done` — so the author is still there, and delivery
//! is the same [`Runtime::prompt`] every dispatch brief uses. The missing piece
//! is only to notice the comment and decide it is worth delivering.
//!
//! ## The loop, and what closes it here
//!
//! The reviewer and the agent act as the same GitHub identity — both are
//! the operator's token — so "skip my own comments" has nothing to key on. A
//! naive implementation delivers a comment, the agent replies on the PR, the
//! board sees a new comment and delivers it back, forever.
//!
//! herdr-board closed that loop with three mechanisms. Only the first survives
//! the port, and the other two are dropped *on purpose* (§review-delivery):
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
//! ## Where one PR → one chat stops being enough (§gh#289)
//!
//! The contract above is one pull request, one authoring attempt, one chat. In a
//! stack that is incomplete: `changes requested` on layer 2 is not only about
//! layer 2, because layers 3..N are built on code that is now wrong. Layer 2's
//! agent gets the review and starts fixing; when it force-pushes, GitHub leaves
//! every layer above it on the old base. The direct child must launch one
//! ordered upstack replay that carries the whole path above it forward before
//! those diffs are reviewable again.
//!
//! So this module addresses the other layers too, in one direction only:
//! [`Dependents`] is the edge, and it points up. Each dependent gets a *notice*
//! and never the review body — the review is not theirs to answer, and pasting
//! it into their chats is how five agents end up editing one branch. The direct
//! child gets the actionable [`compose_notice`]; every layer farther up
//! gets [`compose_hold_notice`] and waits for that one ordered replay to reach
//! it. That is different from a lower layer merging, when GitHub does replay the
//! upper branch and [`crate::rebased::rewritten_notice`] tells its author to
//! bring the checkout across. Two more things follow, and both are about the
//! human rather than the agent:
//!
//! - **The row leaves review.** A diff that is about to be rebased is not
//!   reviewable in good faith, and the bad outcome is not wasted reading, it is
//!   an approval that outlives the diff it was given to. That derives from
//!   [`Delivered::changes_requested`] through the task row — see
//!   [`crate::model::derive_state`].
//! - **An undeliverable notice is still delivered.** A reaped child chat means
//!   there is no agent to tell; it does not mean there is nobody to tell. The row
//!   carries the fact either way, worded by
//!   [`comet_proto::view::board::landing_note`], because the review screen is
//!   where the person who was about to read that diff is looking.
//!
//! Approvals do not fan out. An N-layer stack would generate O(N²) notices over
//! its life, every one of them "somebody below you is fine" and none of them
//! actionable; what a child needs to hear is that its parent *merged*, and
//! §gh#288 and gh#286 own that.
//!
//! What else survives unchanged: the `updated_at` gate (the PR list already
//! says whether anything happened, so the steady state costs no comment
//! fetches — with the base folded into it, §gh#288, so a stack retargeting
//! itself is not mistaken for news), the first-sight floor (never deliver a
//! PR's back catalogue), the actionability filter (the board's own writeback
//! comments, empty approvals), and the author check — the chat must still
//! exist and its cwd must still be the attempt's checkout, because delivering
//! somebody else's review into an unrelated session is worse than not
//! delivering.
//!
//! The author check gates the delivery and nothing else (gh#409). The fetch
//! runs for every row still in `review` with an open pull request, because the
//! standing verdict it computes is not a message: it is the row's own fact,
//! read by the layers stacked on the pull request and by the human about to
//! open the diff. A pull request nobody dispatched (§gh#344) has no chat and
//! never will — gating the fetch on one left such a row unable to acquire a
//! verdict at all, so a stack of them propagated nothing.

use crate::db::Db;
use crate::model::{Attempt, BoardState, Task};
use crate::runtime::Runtime;
use crate::sources::github::{Feedback, FeedbackKind, Github, PullRequest, Rest};
use crate::stacks::Dependents;
use crate::sync::SyncEngine;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
    /// The branch the pull request merged into when those watermarks were
    /// computed (§gh#288).
    ///
    /// The other half of the poll-cost gate above. When a stack's lower layer
    /// merges, GitHub retargets every pull request above it, and each retarget
    /// bumps `updated_at` with nothing on any comment endpoint to show for it —
    /// so `updated_at` alone answers "did anything happen?" with yes for a
    /// five-PR stack's worth of pure waste. Recorded here, a move in the base
    /// accounts for the move in the timestamp.
    ///
    /// Empty means unknown rather than "no base": state written before this
    /// field existed, which is why the first tick after an upgrade fetches.
    #[serde(default)]
    pub base_ref: String,
    /// Whether a missing chat has already been logged, so a task whose chat the
    /// operator archived does not write the same line every 30 seconds.
    #[serde(default)]
    pub noted_gone: bool,
    /// The review that last asked this pull request to change, and which nothing
    /// has withdrawn since (§gh#289). `None` is "nothing outstanding" — never
    /// asked, or asked and since approved.
    ///
    /// Computed off the *fetched* feedback rather than the delivered subset,
    /// because an approval with nothing written in it is not worth an agent's
    /// attention ([`is_actionable`] drops it) and is still the thing that says
    /// the objection is over. Kept here rather than only on the row for the
    /// reason the watermarks are: it is what GitHub last said about this pull
    /// request, next to the ids it was read from — and it is what the fan-out
    /// below is watermarked against. [`store`] mirrors it onto the task row,
    /// where the state derivation and every board read can see it for free.
    #[serde(default)]
    pub changes_requested: Option<i64>,
    /// Per dependent layer, the id of the last review of *this* pull request
    /// fanned out to it (§gh#289).
    ///
    /// Per edge rather than one watermark for the whole fan, so the answer to
    /// "has this layer been told?" is a fact about that layer: a dependent whose
    /// chat was unreachable this cycle is retried, and a layer dispatched onto
    /// this branch after the review landed still hears about it. One watermark
    /// would call the whole fan done on the strength of the first success.
    #[serde(default)]
    pub fanned_out: BTreeMap<String, i64>,
    /// Verdicts submitted from the review window (§gh#239), oldest first.
    ///
    /// Outbound work in an inbound record on purpose: it is the same fact about
    /// the same pull request — what has already been said to this agent about
    /// this work — and the id of a verdict posted from here is written into
    /// [`Delivered::review`] above, so the two halves cannot disagree about
    /// what has been delivered.
    #[serde(default)]
    pub submissions: Vec<crate::verdict::Submission>,
    /// When a review last actually reached this task's chat (gh#563) — an
    /// inbound batch queued into the authoring chat, or an outbound verdict
    /// delivered into it. `None` is "never", and state written before this
    /// field existed reads the same way.
    ///
    /// The one timestamp in this record, because it answers a question the
    /// watermarks cannot: did anything just interrupt the agent? The merge
    /// command asks it, for the window where an answer to that very review can
    /// land on a branch the merge has already closed.
    #[serde(default)]
    pub last_delivery: Option<String>,
}

/// How long after a delivery a merge still warns that the agent may answer it
/// (gh#563).
///
/// A verdict lands; the agent wakes, works, pushes minutes later; the merge has
/// by then closed the branch. Ten minutes is the stretch where that answer is
/// most likely — long enough to cover a small fix, short enough that a warning
/// about it stays current rather than archaeological.
pub(crate) const ANSWER_WINDOW_SECS: i64 = 600;

/// How long ago a review last reached this task's chat, when that is inside
/// [`ANSWER_WINDOW_SECS`] — humanized, ready for a sentence. `None` is "not
/// recently", which includes never and every unreadable stamp: a warning that
/// cannot prove it is current does not fire.
pub(crate) fn recent_delivery(db: &Db, task_id: &str) -> Option<String> {
    let stamp = load(db, task_id).last_delivery?;
    let then = chrono::DateTime::parse_from_rfc3339(&stamp).ok()?;
    let ago = (chrono::Utc::now() - then.with_timezone(&chrono::Utc)).num_seconds();
    (0..=ANSWER_WINDOW_SECS)
        .contains(&ago)
        .then(|| format!("{}s ago", crate::overrun::human_secs(ago)))
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

    /// Adopt whatever verdict this batch of feedback ends on (§gh#289).
    ///
    /// Only `changes_requested`, `approved` and `dismissed` count. A `commented`
    /// review says nothing about whether the objection stands — GitHub's own
    /// review decision treats it the same way — and swallowing an outstanding
    /// request because somebody wrote a paragraph afterwards is the failure this
    /// whole file is about, one layer up.
    ///
    /// `dismissed` counts as a clear (gh#411): GitHub rewrites the dismissed
    /// review's own state in place rather than appending anything, so the
    /// rewritten review is the *only* record that the objection was withdrawn.
    /// Skipping it would leave the standing request pointing at a review that no
    /// longer requests anything — and, since a bot reviewer is refused `APPROVE`
    /// outright, leave merging the pull request as the only way out of the
    /// derived `blocked` above it.
    ///
    /// A batch with no verdict in it leaves the standing one alone: nothing was
    /// said, so nothing changed. Read off the raw fetch and not off the delivered
    /// subset, because an approval with no prose interrupts nobody and is still
    /// what ends the objection.
    fn record_verdict(&mut self, feedback: &[Feedback]) {
        let Some(last) = feedback.iter().rfind(|f| {
            f.kind == FeedbackKind::Review
                && matches!(
                    f.state.as_deref(),
                    Some("changes_requested" | "approved" | "dismissed")
                )
        }) else {
            return;
        };
        self.changes_requested = last.requests_changes().then_some(last.id);
    }
}

/// What the pass decided about one pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Nothing has happened on the pull request since the last look. Costs no
    /// GitHub call at all.
    Unchanged,
    /// The pull request moved, and a retarget accounts for it: the base branch
    /// is not the one the watermarks were computed against, which is what a
    /// lower layer of a stack merging does to every layer above it. Costs no
    /// GitHub call either (§gh#288).
    Retargeted,
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
    // A dismissed review's body is the objection somebody explicitly withdrew.
    // It is in the fetch for [`Delivered::record_verdict`]'s sake (gh#411), and
    // delivering it would hand an agent work that no longer stands.
    if f.state.as_deref() == Some("dismissed") {
        return false;
    }
    // A verdict of `changes requested` is actionable with or without prose. An
    // approval without prose is not: it says the agent has nothing left to do,
    // and interrupting it to hear so is the opposite of useful.
    f.requests_changes() || !f.body.trim().is_empty()
}

pub(crate) fn is_the_boards_own(body: &str) -> bool {
    let b = body.trim_start();
    b.starts_with("comet-board:")
        || b.starts_with("Dispatched to comet")
        // A verdict written in the review window (§gh#239). Its id is
        // watermarked the moment it is posted, so this is the backstop and not
        // the mechanism — it is what keeps the relay shut when GitHub answers
        // the post without an id to watermark.
        || b.contains(crate::verdict::POSTED_MARK)
}

/// The census a delivery headline can open with: how many of these items are
/// review verdicts and how many are comments (inline or on the conversation).
/// The split is worth keeping because a verdict speaks for a whole review pass
/// while a comment speaks for one line of it.
pub fn census(items: &[Feedback]) -> (usize, usize) {
    (
        items
            .iter()
            .filter(|f| f.kind == FeedbackKind::Review)
            .count(),
        items.iter().filter(|f| f.kind != FeedbackKind::Review).count(),
    )
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
/// nothing written before the agent finished can be a review of its work. A
/// pull request nobody dispatched has no attempt and nothing is delivered for
/// it, so its caller passes an empty floor, which sits below every timestamp.
pub fn plan_delivery(
    state: &mut Delivered,
    pr_updated_at: &str,
    pr_base_ref: &str,
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

    // The same gate, with the base folded in (§gh#288). A retarget is the one
    // thing that moves `updated_at` while leaving all three comment endpoints
    // exactly as they were, and a merged stack does it to every layer above the
    // one that landed — four PRs times three endpoints, per merge, all of them
    // answering `NothingNew`.
    //
    // Imperfect on purpose, and the imperfection is a delay rather than a loss:
    // a comment that shares its poll window with a retarget is not fetched, but
    // it is not consumed either, so it is still above its watermark. It does not
    // arrive on the next tick — `updated_at` is advanced to the retarget's own
    // stamp, so the gate above answers `Unchanged` until *something else moves
    // the pull request*, and the first tick after that delivers it. Leaving the
    // stamp behind instead would fetch one tick later and pay the very calls
    // this gate exists to remove. §gh#288 states the residual risk in full.
    //
    // Never on first sight: an unknown base differs from every real one, and
    // skipping the first look would leave the floor unapplied and hand the
    // agent the whole back catalogue the next time anything moved.
    let retargeted = !first_sight
        && !state.base_ref.is_empty()
        && !pr_base_ref.is_empty()
        && state.base_ref != pr_base_ref;
    if retargeted {
        state.base_ref = pr_base_ref.to_string();
        state.updated_at = pr_updated_at.to_string();
        return Ok(Decision::Retargeted);
    }

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
    // The verdict is not a message, so the floor and the actionability filter
    // above have no business in it: a `changes requested` that predates the
    // attempt's end is still outstanding, and an approval nobody wrote a word in
    // still withdraws one (§gh#289).
    state.record_verdict(&feedback);
    state.updated_at = pr_updated_at.to_string();
    state.base_ref = pr_base_ref.to_string();

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

/// One layer of a stack, resolved to somewhere the board can deliver (§gh#289).
///
/// The mechanism, rather than a special case for children: "address layer N of
/// this stack" is one question, asked of the layer that was reviewed and of every
/// layer built on it in exactly the same words. Whatever the review surface turns
/// out to want (#281), it wants this and not a bespoke notice path.
#[derive(Debug, Clone, Copy)]
pub struct Addressed<'a> {
    /// The attempt that wrote the pull request — the branch is the proof.
    pub attempt: &'a Attempt,
    /// Its chat, verified alive and still standing in the attempt's checkout.
    pub chat_id: &'a str,
}

/// Where a row's pull request can be reached, if it can be.
///
/// `Ok(None)` is a settled fact: nobody dispatched this pull request, or the chat
/// that wrote it is gone, or it has been re-pointed at another checkout. An `Err`
/// is the runtime failing to answer, which is not the same thing — the caller
/// leaves its state unwritten and the next cycle asks again.
///
/// The whole author check in one place, so the fan-out cannot drift from the
/// delivery it is a fan-out of. A server-side rebase does not move a chat out of
/// its checkout: [`still_the_authors_checkout`] compares a filesystem path, not a
/// ref.
pub fn address<'a>(
    runtime: &dyn Runtime,
    task: &'a Task,
    pr: &PullRequest,
) -> Result<Option<Addressed<'a>>> {
    let Some(attempt) = authoring_attempt(task, pr) else {
        return Ok(None);
    };
    let Some(chat_id) = attempt.pane_id.as_deref() else {
        return Ok(None);
    };
    if !runtime.chat_alive(chat_id)?
        || !still_the_authors_checkout(runtime.chat_cwd(chat_id)?.as_deref(), attempt)
    {
        return Ok(None);
    }
    Ok(Some(Addressed { attempt, chat_id }))
}

/// How every delivery into an authoring chat opens: who is talking, which task,
/// and the pull request it is about.
///
/// Shared with the outbound half (§gh#239) rather than written twice. An agent
/// reads relayed feedback and a verdict written in the review window in the
/// same conversation, and two spellings of the same four lines would read as
/// two different systems talking.
pub(crate) fn reviewed_header(identifier: &str, title: &str, pr_url: &str) -> String {
    format!(
        "comet-board: your pull request has been reviewed.\n\n  {identifier} · {title}\n  {pr_url}\n"
    )
}

/// The two sentences that say where the fix goes. Shared with the outbound half
/// for the same reason [`reviewed_header`] is.
pub(crate) const STILL_OPEN: &str =
    "The pull request is still open and you are still in the checkout you wrote it in.";
pub(crate) const ADDRESS_IT: &str =
    "Address the feedback there, commit, and push to the same branch";

/// The message an agent's chat is queued with.
///
/// Enough to act on without a lookup: the body, the pull request URL, and for
/// an inline comment the file and line it hangs off. The tail states the one
/// consequence of author-blind delivery — a comment the agent itself wrote may
/// come back once — so recognising the relay takes no cleverness.
pub fn compose(task: &Task, pr: &PullRequest, items: &[Feedback]) -> String {
    let mut s = reviewed_header(&task.identifier, &task.title, &pr.url);
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
    s.push_str(&format!(
        "\n{STILL_OPEN} {ADDRESS_IT} — do not open a second pull request. If you disagree \
         with a point, say so on the pull request rather than silently skipping it. The \
         board cannot tell comment authors apart, so a comment you write on the pull \
         request may be relayed back to you once — if a comment above is your own, it \
         needs nothing from you.\n",
    ));
    s
}

/// What the direct child is told when the layer below it is asked to change
/// (§gh#289, gh#407).
///
/// **A notice, not the review.** The review below is not this agent's to answer:
/// it is about code on another branch, and an agent handed somebody else's
/// feedback either edits a branch that is not its own or replies on a pull
/// request that is not its own. What it needs is the one fact its own chat cannot
/// see — that the ground under its branch is about to move, and that it owns the
/// one ordered replay which carries every layer above it across too.
///
/// It opens in [`reviewed_header`]'s register but deliberately not with its
/// sentence: "your pull request has been reviewed" would be false, and an agent
/// that reads it as true goes looking for feedback that is not there.
pub fn compose_notice(
    task: &Task,
    pr: &PullRequest,
    below: &Task,
    below_pr: &PullRequest,
) -> String {
    format!(
        "comet-board: the layer below yours was asked to change.\n\n  \
         {below_id} · {title}\n  {below_url} · {below_branch}\n\n\
         Your own pull request — {mine} · PR #{number} on {branch} — is stacked on \
         {below_branch}. When its author force-pushes the fix, GitHub leaves your \
         branch on the old history and your pull request may become dirty. This is \
         different from a lower-layer merge: after a merge, GitHub replays the \
         branches above it itself.\n\n\
         The review below is not yours to answer — do not write on {below_url} or \
         touch {below_branch}. You own the ordered replay for this path through the \
         stack. Wait for its author to push the fix; then, from your own branch, run \
         `gh stack rebase --upstack` to carry this layer and every layer above it \
         forward. Layers farther up are waiting on this replay; their authors are not \
         asked to start their own. Your diff moves when that replay is pushed, and so \
         does whatever a reviewer of yours has already read. If you are mid-task, \
         carry on and commit as you go: the rebase carries committed work across, and \
         uncommitted work in your worktree is yours to keep safe.\n",
        below_id = below.identifier,
        title = below.title,
        below_url = below_pr.url,
        below_branch = below_pr.head_ref,
        mine = task.identifier,
        number = pr.number,
        branch = pr.head_ref,
    )
}

/// What a transitive dependent is told when a layer farther below it is asked
/// to change (gh#407).
///
/// Still a notice rather than the review, but deliberately not an instruction
/// to rebase: the direct child owns that command and cascades it through this
/// layer. Giving every author the same command races the ordered replay and can
/// put an upper layer onto a still-stale parent.
pub fn compose_hold_notice(
    task: &Task,
    pr: &PullRequest,
    below: &Task,
    below_pr: &PullRequest,
) -> String {
    format!(
        "comet-board: a lower layer in your stack was asked to change.\n\n  \
         {below_id} · {title}\n  {below_url} · {below_branch}\n\n\
         Your own pull request — {mine} · PR #{number} on {branch} — sits above \
         {below_branch} in the same stack. When its author force-pushes the fix, \
         GitHub leaves the upper branches on their old history and their pull \
         requests may become dirty.\n\n\
         The direct child on your path above {below_branch} owns the ordered replay \
         and will carry your layer forward with the layers between them. Do not start \
         a separate rebase from this branch or race that replay. The review below is \
         not yours to answer — do not write on {below_url} or touch {below_branch}. \
         If you are mid-task, carry on and commit as you go: the rebase carries \
         committed work across, and uncommitted work in your worktree is yours to \
         keep safe.\n",
        below_id = below.identifier,
        title = below.title,
        below_url = below_pr.url,
        below_branch = below_pr.head_ref,
        mine = task.identifier,
        number = pr.number,
        branch = pr.head_ref,
    )
}

/// Load a task's delivery state. A missing or unreadable value starts over,
/// which costs one round of re-consumption and never a wrong delivery.
pub(crate) fn load(db: &Db, task_id: &str) -> Delivered {
    db.meta_get(&crate::sync::meta::reviews_for(task_id))
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

/// Persist a task's delivery state — and mirror the one field of it that other
/// rows read onto the task row (§gh#289).
///
/// Every writer of [`Delivered`] goes through here, which is what stops the record
/// and the column disagreeing about what a reviewer last said. The record is the
/// source; the column exists so the state derivation and every board read can see
/// it without a `meta` lookup per row.
pub(crate) fn store(db: &Db, task_id: &str, state: &Delivered) -> Result<()> {
    db.meta_set(
        &crate::sync::meta::reviews_for(task_id),
        &serde_json::to_string(state)?,
    )?;
    db.set_pr_changes_requested(task_id, state.changes_requested)?;
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
        // The dependency edges, once for the whole board: the layers built on a
        // row are other rows, and asking per task would be a scan per task
        // (§gh#289).
        let deps = Dependents::of(&tasks);
        let mut verdicts_moved = false;
        for task in &tasks {
            match self.deliver_review_for(gh, runtime, pulls, &tasks, &deps, task) {
                Ok(moved) => verdicts_moved |= moved,
                Err(e) => self
                    .log
                    .warn(format!("delivering review for {}: {e}", task.identifier)),
            }
        }
        // A standing `changes requested` is what takes the layers above it out of
        // `review` (§gh#289), and this pass runs *after* the cycle has already
        // derived every row. Without this the rows above would sit in `review`
        // for one more poll interval — reviewable-looking for exactly as long as
        // it takes somebody to open one.
        if verdicts_moved && let Err(e) = self.rederive_all() {
            self.log
                .warn(format!("rederiving after a review verdict moved: {e}"));
        }
    }

    fn deliver_review_for(
        &self,
        gh: &Github<Box<dyn Rest>>,
        runtime: &dyn Runtime,
        pulls: &[PullRequest],
        tasks: &[Task],
        deps: &Dependents,
        task: &Task,
    ) -> Result<bool> {
        // The fan-out first, and on a gate of its own: it is about the layers
        // *above* this row, so whether this row is still in `review` and whether
        // its own chat is still there have no bearing on it (§gh#289). Costs no
        // GitHub call — the standing verdict is already on the row — and nothing
        // at all for the overwhelming majority of rows, which have no outstanding
        // request and no dependents.
        if task.pr_changes_requested.is_some() {
            self.fan_out_changes(runtime, pulls, tasks, deps, task)?;
        }

        // `review` is the whole precondition for the rest: finished work with an
        // open pull request.
        if task.state != BoardState::Review || !task.pr_open {
            return Ok(false);
        }
        let Some(pr) = pull_request_for(task, pulls) else {
            return Ok(false);
        };
        let mut state = load(&self.db, &task.id);

        // Who the delivery would go to — and only the delivery (gh#409). The
        // fetch below is not gated on an author, because the standing verdict
        // it computes is not a message: it is the row's own fact, the one the
        // layers stacked on this pull request and the human about to open the
        // diff both read. Gating the fetch on a chat was the bug — an
        // undispatched pull request could never acquire a verdict, so a stack
        // of them propagated nothing.
        let attempt = authoring_attempt(task, pr);
        let author = match attempt {
            // A pull request nobody dispatched has no author sitting in a chat,
            // and no chat to have lost: it is not the gone case below, just a
            // row with nobody to prompt.
            None => None,
            // A task whose chat is gone is delivered to nobody, quietly. There
            // is nothing to do about that — re-dispatching would be a second
            // agent on work that is already written. A runtime error inside
            // `address` propagates instead: not knowing is not the same as
            // gone, and the next cycle asks again.
            Some(_) => match address(runtime, task, pr)? {
                Some(author) => {
                    state.noted_gone = false;
                    Some(author)
                }
                None => {
                    if !state.noted_gone {
                        self.log.info(format!(
                            "{}: no live chat still holds the agent that wrote {} — \
                             review comments will not be delivered",
                            task.identifier, pr.url
                        ));
                        state.noted_gone = true;
                        store(&self.db, &task.id, &state)?;
                    }
                    None
                }
            },
        };

        // The floor exists to stop a first look handing an agent its pull
        // request's back catalogue. With no attempt there is no catalogue to
        // withhold — nothing is delivered at all — and an empty floor sits
        // below every timestamp.
        let floor = attempt
            .map(|a| a.ended_at.clone().unwrap_or_else(|| a.started_at.clone()))
            .unwrap_or_default();

        let decision = plan_delivery(&mut state, &pr.updated_at, &pr.base_ref, &floor, || {
            gh.pr_feedback(&pr.repo, pr.number)
        })?;

        if let Decision::Deliver(items) = &decision {
            if let Some(author) = &author {
                let chat_id = author.chat_id;
                let text = compose(task, pr, items);
                if let Err(e) = runtime.prompt(chat_id, &text) {
                    // Nothing arrived, so nothing was consumed: dropping the state
                    // here is what makes the next cycle try again.
                    self.log.warn(format!(
                        "{}: could not queue {} review comment(s) into chat {chat_id}: {e}",
                        task.identifier,
                        items.len()
                    ));
                    return Ok(false);
                }
                self.log.info(format!(
                    "{}: delivered {} review comment(s) on {} into chat {chat_id}",
                    task.identifier,
                    items.len(),
                    pr.url
                ));
                // The merge side asks this later (gh#563): a verdict that went
                // into the chat minutes ago is the one whose answer can land
                // after its pull request closed.
                state.last_delivery = Some(crate::db::now());
            } else {
                // Consumed unspoken: there is no chat to hand these to, and
                // holding them back would hold the verdict back with them. The
                // row carries what matters.
                self.log.info(format!(
                    "{}: {} review comment(s) on {} had no chat to go to — the row \
                     carries the verdict",
                    task.identifier,
                    items.len(),
                    pr.url
                ));
            }
        }
        if decision == Decision::Retargeted {
            // Said once per retarget, so an operator watching a stack land can
            // see why a moved pull request asked GitHub nothing.
            self.log.info(format!(
                "{}: {} was retargeted onto {} — nothing to fetch",
                task.identifier, pr.url, pr.base_ref
            ));
        }
        store(&self.db, &task.id, &state)?;
        // A verdict this cycle's own fetch discovered fans out in this cycle.
        // The gate at the top read the row as the cycle loaded it, which cannot
        // know what the fetch has since found — and a stack should not wait a
        // poll interval to hear that its foundation is moving.
        let moved = state.changes_requested != task.pr_changes_requested;
        if moved && state.changes_requested.is_some() {
            self.fan_out_changes(runtime, pulls, tasks, deps, task)?;
        }
        Ok(moved)
    }

    /// Tell every layer built on this pull request that it has been asked to
    /// change (§gh#289): its direct child owns the ordered replay, while every
    /// transitive dependent waits for that replay to reach it (gh#407).
    ///
    /// Runs off the standing verdict rather than off a review just read, so the
    /// two ways a `changes requested` reaches the board — noticed on GitHub by
    /// the pass above, or written in the review window (§gh#239) — fan out
    /// through one path. Costs no GitHub call: everything it needs is the pull
    /// requests the cycle already polled and the rows already loaded.
    ///
    /// A dependent the board cannot reach is not a dependent the board says
    /// nothing about. The notice into its chat is one of two deliveries; the
    /// other is the row itself, which carries the fact for the human who was
    /// about to review that diff whether or not an agent is still sitting in it.
    /// So an unreachable chat is recorded as told — the row has it — and a chat
    /// the *ledger* refused is not, because that is a failure to retry.
    fn fan_out_changes(
        &self,
        runtime: &dyn Runtime,
        pulls: &[PullRequest],
        tasks: &[Task],
        deps: &Dependents,
        task: &Task,
    ) -> Result<()> {
        if !task.pr_open {
            return Ok(());
        }
        let above = deps.above(&task.id);
        if above.is_empty() {
            return Ok(());
        }
        let mut state = load(&self.db, &task.id);
        let Some(review_id) = state.changes_requested else {
            return Ok(());
        };
        let Some(pr) = pull_request_for(task, pulls) else {
            return Ok(());
        };
        let mut told = 0;
        let mut recorded = 0;
        for dep in above {
            if state
                .fanned_out
                .get(&dep.id)
                .is_some_and(|at| *at >= review_id)
            {
                continue;
            }
            // A layer that has landed or been withdrawn is not going to be
            // replayed on anything. Recorded as told rather than skipped, so it
            // is not reconsidered every cycle for the rest of its life.
            let Some(child) = tasks.iter().find(|t| t.id == dep.id).filter(|t| t.pr_open) else {
                state.fanned_out.insert(dep.id.clone(), review_id);
                recorded += 1;
                continue;
            };
            let Some(child_pr) = pull_request_for(child, pulls) else {
                state.fanned_out.insert(dep.id.clone(), review_id);
                recorded += 1;
                continue;
            };
            match address(runtime, child, child_pr)? {
                Some(at) => {
                    let text = if deps.is_direct_child(&task.id, &dep.id) {
                        compose_notice(child, child_pr, task, pr)
                    } else {
                        compose_hold_notice(child, child_pr, task, pr)
                    };
                    if let Err(e) = runtime.prompt(at.chat_id, &text) {
                        // Nothing arrived, so nothing is recorded: the next cycle
                        // tries this dependent again, and the ones already told
                        // are not told twice.
                        self.log.warn(format!(
                            "{}: could not tell {} that {} was asked to change: {e}",
                            task.identifier, child.identifier, pr.url
                        ));
                        continue;
                    }
                    told += 1;
                }
                None => self.log.info(format!(
                    "{}: {} is stacked on {} and has no live chat to tell — the row \
                     carries it instead",
                    task.identifier, child.identifier, pr.url
                )),
            }
            state.fanned_out.insert(dep.id.clone(), review_id);
            recorded += 1;
        }
        if told > 0 {
            self.log.info(format!(
                "{}: told {told} layer(s) above it that {} was asked to change",
                task.identifier, pr.url
            ));
        }
        // Nothing new to remember means nothing to write: a stack sitting on a
        // standing request is the steady state, and it should cost this pass no
        // rows at all.
        if recorded > 0 {
            store(&self.db, &task.id, &state)?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
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

    /// The base a pull request outside a stack keeps for its whole life, so a
    /// test that is not about retargeting never has to mention one.
    const BASE: &str = "main";

    #[test]
    fn census_splits_verdicts_from_comments() {
        let items = vec![
            verdict(1, "changes_requested", "Two things.", FLOOR),
            inline(2, "crates/board/src/review.rs", 12, "typo", FLOOR),
            feedback(FeedbackKind::Issue, 3, "also this", FLOOR),
            verdict(4, "approved", "ship it", FLOOR),
        ];
        assert_eq!(census(&items), (2, 2));
    }

    #[test]
    fn census_of_nothing_is_zeroed() {
        assert_eq!(census(&[]), (0, 0));
        let only_comments = vec![feedback(FeedbackKind::Issue, 1, "hi", FLOOR)];
        assert_eq!(census(&only_comments), (0, 1));
    }

    fn plan(state: &mut Delivered, updated_at: &str, items: Vec<Feedback>) -> Decision {
        plan_delivery(state, updated_at, BASE, FLOOR, || Ok(items)).unwrap()
    }

    /// A pull request the board has already looked at once, watermarks and all.
    fn seen(updated_at: &str) -> Delivered {
        Delivered {
            updated_at: updated_at.into(),
            base_ref: BASE.into(),
            ..Default::default()
        }
    }

    /// The steady state, and the one that has to be free: the pull request has
    /// not moved, so none of the three comment endpoints is asked at all.
    #[test]
    fn an_unchanged_pull_request_costs_no_api_call() {
        let mut state = seen("2026-07-28T11:00:00Z");
        let decision = plan_delivery(&mut state, "2026-07-28T11:00:00Z", BASE, FLOOR, || {
            panic!("nothing moved on the PR; there is nothing to ask about")
        })
        .unwrap();
        assert_eq!(decision, Decision::Unchanged);
    }

    #[test]
    fn a_changes_requested_review_is_delivered_and_watermarked() {
        let mut state = seen("2026-07-28T11:00:00Z");
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
        let mut state = seen("2026-07-28T11:00:00Z");
        // The reviewer asks for changes.
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
        // reviewer, so nothing about the comment says who wrote it — it is
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
        let d = plan_delivery(&mut state, "2026-07-28T11:40:00Z", BASE, FLOOR, || {
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

    /// What a stack does to this gate (§gh#288): the layer below merges,
    /// GitHub retargets this pull request onto it, `updated_at` moves, and
    /// there is nothing on any of the three endpoints to show for it.
    #[test]
    fn a_retarget_moves_the_timestamp_and_costs_no_api_call() {
        let mut state = Delivered {
            updated_at: "2026-07-28T11:00:00Z".into(),
            base_ref: "board/gh-287".into(),
            ..Default::default()
        };
        let d = plan_delivery(&mut state, "2026-07-28T11:30:00Z", "main", FLOOR, || {
            panic!("a retarget is not news; there is nothing to ask about")
        })
        .unwrap();
        assert_eq!(d, Decision::Retargeted);
        // Both halves of the gate move, or the next tick pays the cost anyway.
        assert_eq!(state.updated_at, "2026-07-28T11:30:00Z");
        assert_eq!(state.base_ref, "main");

        // And the pull request is still watched: a real review on the new base
        // is fetched and delivered.
        let d = plan_delivery(&mut state, "2026-07-28T11:40:00Z", "main", FLOOR, || {
            Ok(vec![verdict(
                900,
                "changes_requested",
                "Rebase left a stray import.",
                "2026-07-28T11:40:00Z",
            )])
        })
        .unwrap();
        assert!(matches!(d, Decision::Deliver(items) if items.len() == 1));
    }

    /// The accepted imperfection: a comment that lands inside the same poll
    /// window as the retarget is not fetched then — but it was never consumed,
    /// so it is still above its watermark. Not the next *tick*: the retarget
    /// advanced `updated_at`, so what recovers it is the next *event* on the
    /// pull request, which is why the clock below steps to one.
    #[test]
    fn a_comment_that_shares_a_window_with_a_retarget_is_delivered_late_not_lost() {
        let mut state = seen("2026-07-28T11:00:00Z");
        let comment = feedback(
            FeedbackKind::Issue,
            41,
            "This one broke the build.",
            "2026-07-28T11:29:00Z",
        );

        let d = plan(&mut state, "2026-07-28T11:30:00Z", vec![comment.clone()]);
        assert_eq!(
            d,
            Decision::Deliver(vec![comment.clone()]),
            "no stack, no gate"
        );

        // The same window, on a stacked pull request that was retargeted: not
        // asked, so nothing is consumed.
        let mut state = seen("2026-07-28T11:00:00Z");
        let d = plan_delivery(
            &mut state,
            "2026-07-28T11:30:00Z",
            "board/gh-287",
            FLOOR,
            || Ok(vec![comment.clone()]),
        )
        .unwrap();
        assert_eq!(d, Decision::Retargeted);
        assert_eq!(state.issue, 0, "unasked is not consumed");

        // Every following tick over a pull request nothing else has touched is
        // `Unchanged` — the delay is until the next *event*, not the next tick,
        // and saying otherwise would be softer than this is.
        let d = plan_delivery(
            &mut state,
            "2026-07-28T11:30:00Z",
            "board/gh-287",
            FLOOR,
            || panic!("the retarget's stamp was recorded; this is the ordinary gate"),
        )
        .unwrap();
        assert_eq!(d, Decision::Unchanged);

        // The next move of the pull request — with the base standing still —
        // hands it over.
        let d = plan_delivery(
            &mut state,
            "2026-07-28T12:00:00Z",
            "board/gh-287",
            FLOOR,
            || Ok(vec![comment.clone()]),
        )
        .unwrap();
        assert_eq!(d, Decision::Deliver(vec![comment]));
    }

    /// State written before the base was folded in (§gh#288) knows no base at
    /// all. Unknown is not "changed": the first tick after the upgrade fetches
    /// as it always did, and records the base it compared nothing against.
    #[test]
    fn a_record_from_before_the_base_was_stored_is_not_read_as_a_retarget() {
        let mut state = Delivered {
            updated_at: "2026-07-28T11:00:00Z".into(),
            ..Default::default()
        };
        assert!(state.base_ref.is_empty());
        let d = plan(
            &mut state,
            "2026-07-28T11:30:00Z",
            vec![feedback(
                FeedbackKind::Issue,
                41,
                "Still wrong on the retry path.",
                "2026-07-28T11:30:00Z",
            )],
        );
        assert!(matches!(d, Decision::Deliver(items) if items.len() == 1));
        assert_eq!(state.base_ref, BASE, "and the gate is armed from here on");
    }

    /// First sight has no base to compare against either — and skipping it
    /// would be worse than unthrifty: the floor applies only on first sight, so
    /// a skipped first look would hand the agent the whole back catalogue the
    /// next time anything moved.
    #[test]
    fn the_first_look_at_a_pull_request_is_never_a_retarget() {
        let mut state = Delivered::default();
        let d = plan_delivery(
            &mut state,
            "2026-07-28T12:00:00Z",
            "board/gh-287",
            FLOOR,
            || {
                Ok(vec![feedback(
                    FeedbackKind::Issue,
                    10,
                    "Opened the PR.",
                    "2026-07-28T09:30:00Z",
                )])
            },
        )
        .unwrap();
        assert_eq!(d, Decision::NothingNew, "asked, and floored");
        assert_eq!(state.issue, 10, "consumed, so it can never come back");
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
        // A dismissed review's body is an objection somebody withdrew, so its
        // prose is exactly what must not be handed to an agent (gh#411).
        assert!(!is_actionable(&verdict(
            5,
            "dismissed",
            "Split the watermark per endpoint.",
            "t"
        )));
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

    pub(crate) fn task_row() -> Task {
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
            upstream: UpstreamState::Unstarted,
            local_done: false,
            pr_url: Some("https://github.com/o/r/pull/14".into()),
            pr_number: Some(14),
            pr_open: true,
            pr_merged: false,
            pr_mergeable: None,
            pr_base_ref: None,
            pr_head_ref: None,
            pr_stack: None,
            pr_changes_requested: None,
            updated_at: "t".into(),
            synced_at: "t".into(),
            attempts: vec![],
        }
    }

    pub(crate) fn pull(head_ref: &str) -> PullRequest {
        PullRequest {
            repo: "o/r".into(),
            number: 14,
            title: "Deliver review comments".into(),
            body: None,
            url: "https://github.com/o/r/pull/14".into(),
            head_ref: head_ref.into(),
            base_ref: "main".into(),
            head_repo: None,
            base_sha: None,
            open: true,
            merged: false,
            draft: false,
            stack: None,
            updated_at: "2026-07-28T11:30:00Z".into(),
        }
    }

    fn attempt(branch: &str, chat: &str, worktree: Option<&str>) -> Attempt {
        Attempt {
            task_id: "gh:o/r#13".into(),
            pane_id: Some(chat.into()),
            worktree: worktree.map(str::to_string),
            branch: Some(branch.into()),
            started_at: "2026-07-28T09:00:00Z".into(),
            ended_at: Some("2026-07-28T10:00:00Z".into()),
            outcome: Some(Outcome::Done),
            agent_status: Some(AgentStatus::Idle),
            repo_path: Some("/repo/r".into()),
            ..crate::model::tests::blank_attempt()
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
    pub(crate) struct FakeRuntime {
        pub(crate) alive: bool,
        pub(crate) cwd: Option<String>,
        /// Per-chat cwds, for a test with more than one chat in it — a stack has
        /// a chat and a checkout per layer, and one shared cwd would fail every
        /// layer's author check but the first.
        pub(crate) cwds: std::collections::HashMap<String, String>,
        /// Chats that are gone while the rest are not.
        pub(crate) reaped: std::collections::HashSet<String>,
        pub(crate) prompts: std::cell::RefCell<Vec<(String, String)>>,
        pub(crate) prompt_fails: bool,
    }

    impl FakeRuntime {
        pub(crate) fn holding(cwd: &str) -> FakeRuntime {
            FakeRuntime {
                alive: true,
                cwd: Some(cwd.into()),
                cwds: Default::default(),
                reaped: Default::default(),
                prompts: Default::default(),
                prompt_fails: false,
            }
        }

        pub(crate) fn gone() -> FakeRuntime {
            FakeRuntime {
                alive: false,
                cwd: None,
                cwds: Default::default(),
                reaped: Default::default(),
                prompts: Default::default(),
                prompt_fails: false,
            }
        }

        /// Every chat in its own checkout, keyed by chat id.
        pub(crate) fn holding_each(cwds: &[(&str, &str)]) -> FakeRuntime {
            FakeRuntime {
                cwds: cwds
                    .iter()
                    .map(|(c, w)| (c.to_string(), w.to_string()))
                    .collect(),
                ..FakeRuntime::holding("/nowhere")
            }
        }

        /// What one chat was queued with, if it was queued at all.
        pub(crate) fn said_to(&self, chat: &str) -> Option<String> {
            self.prompts
                .borrow()
                .iter()
                .find(|(c, _)| c == chat)
                .map(|(_, t)| t.clone())
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
        fn chat_alive(&self, chat_id: &str) -> Result<bool> {
            Ok(self.alive && !self.reaped.contains(chat_id))
        }
        fn chat_cwd(&self, chat_id: &str) -> Result<Option<String>> {
            Ok(self.cwds.get(chat_id).cloned().or_else(|| self.cwd.clone()))
        }
        fn last_run_end(&self, _chat_id: &str) -> Result<Option<crate::runtime::RunEnd>> {
            Ok(None)
        }
    }

    /// Shares one fixture between the engine, which owns its transport, and the
    /// test, which needs to read back what was asked. Since gh#369 the
    /// reviewer's own client shares it too, so "one review was posted" is a
    /// countable claim however many identities were involved.
    pub(crate) use crate::sources::github::SharedFixture as Shared;

    /// A `SyncEngine` that owns its scratch directory. Dropping the fixture
    /// removes the directory — on the box `/tmp` is RAM, and the old
    /// leave-it-behind helpers had stacked up 151k directories there (gh#430).
    /// Derefs to the engine, so tests read exactly as before.
    pub(crate) struct TestEngine {
        pub(crate) e: SyncEngine,
        pub(crate) _tmp: tempfile::TempDir,
    }

    impl std::ops::Deref for TestEngine {
        type Target = SyncEngine;
        fn deref(&self) -> &SyncEngine {
            &self.e
        }
    }

    impl std::ops::DerefMut for TestEngine {
        fn deref_mut(&mut self) -> &mut SyncEngine {
            &mut self.e
        }
    }

    /// An engine over an in-memory board and a recorded GitHub.
    pub(crate) fn engine(rest: std::rc::Rc<crate::sources::github::FixtureRest>) -> TestEngine {
        let tmp = tempfile::Builder::new()
            .prefix("cb-review-")
            .tempdir()
            .unwrap();
        let dir = tmp.path().to_path_buf();
        let e = SyncEngine {
            db: Db::open_in_memory().unwrap(),
            cfg: crate::config::RoutingConfig::default(),
            credentials: Default::default(),
            paths: crate::config::Paths {
                config_dir: dir.clone(),
                state_dir: dir,
            },
            log: std::sync::Arc::new(crate::log::Logger::new("", false)),
            github: Some(Github::new(Box::new(Shared(rest)) as Box<dyn Rest>)),
            push_capabilities: None,
            // Nothing to hand out until a test says a member holds a token
            // (gh#369); a board that holds none never asks for a client.
            as_user: std::rc::Rc::new(crate::sources::github::FixtureAsUser::default()),
            webhook: std::sync::Arc::new(crate::notify::HttpWebhook),
        };
        TestEngine { e, _tmp: tmp }
    }

    /// A finished attempt on gh#13, with an open pull request — the row this
    /// whole module is about.
    pub(crate) fn seed_reviewed_task(e: &SyncEngine, branch: &str, chat: &str) {
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
            upstream: UpstreamState::Unstarted,
            updated_at: crate::db::now(),
        })
        .unwrap();
        let a =
            e.db.insert_attempt(&crate::db::NewAttempt {
                automation: None,
                automation_owner: None,
                stacked_on: None,
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
                dispatched_by_verified: false,
                billed_to: None,
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

    pub(crate) fn state_of(e: &SyncEngine) -> Delivered {
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
    /// The verdict is another matter (gh#409): the fetch that computes it is
    /// not delivery, and the row still learns what the reviewer said.
    #[test]
    fn a_task_whose_chat_is_gone_is_told_nothing_but_its_row_still_learns() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(feedback_fixture()));
        let e = engine(rest.clone());
        seed_reviewed_task(&e, "board/gh-13", "chat-1");

        let runtime = FakeRuntime::gone();
        e.deliver_reviews(&runtime, &[pull("board/gh-13")]);

        assert!(
            runtime.prompts.borrow().is_empty(),
            "there is nobody to tell: {:?}",
            runtime.prompts.borrow()
        );
        assert!(
            state_of(&e).noted_gone,
            "and it is said once, not every tick"
        );
        assert_eq!(
            e.db.get_task("gh:o/r#13")
                .unwrap()
                .unwrap()
                .pr_changes_requested,
            Some(900),
            "the standing verdict lands on the row anyway",
        );

        // And the steady state still costs nothing: an unchanged pull request
        // asks no endpoint, chat or no chat.
        let asked = rest.asked.borrow().len();
        e.deliver_reviews(&runtime, &[pull("board/gh-13")]);
        assert_eq!(rest.asked.borrow().len(), asked, "the updated_at gate held");
    }

    /// A live chat whose cwd is somebody else's checkout is the same verdict
    /// as a gone chat: the author is not there to deliver to.
    #[test]
    fn a_chat_repointed_at_another_checkout_is_not_delivered_into() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(feedback_fixture()));
        let e = engine(rest.clone());
        seed_reviewed_task(&e, "board/gh-13", "chat-1");

        let runtime = FakeRuntime::holding("/wt/lin-140-1");
        e.deliver_reviews(&runtime, &[pull("board/gh-13")]);

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

    /// The stack case end to end (§gh#288): the layer below lands, GitHub
    /// retargets this pull request, and the cycle that sees it asks nothing.
    /// Repeat that per layer per merge and it is the whole saving.
    #[test]
    fn a_retargeted_pull_request_asks_github_nothing() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(vec![
            (
                "/repos/o/r/issues/14/comments".into(),
                serde_json::json!([]),
            ),
            ("/repos/o/r/pulls/14/comments".into(), serde_json::json!([])),
            ("/repos/o/r/pulls/14/reviews".into(), serde_json::json!([])),
        ]));
        let e = engine(rest.clone());
        seed_reviewed_task(&e, "board/gh-13", "chat-1");
        let runtime = FakeRuntime::holding("/wt/gh-13-1");

        // One look, on a pull request stacked on the layer below.
        let mut pr = pull("board/gh-13");
        pr.base_ref = "board/gh-12".into();
        e.deliver_reviews(&runtime, std::slice::from_ref(&pr));
        assert_eq!(rest.asked.borrow().len(), 3);
        assert_eq!(state_of(&e).base_ref, "board/gh-12");

        // gh#12 merges. GitHub retargets this one onto main and stamps it.
        pr.base_ref = "main".into();
        pr.updated_at = "2026-07-28T12:00:00Z".into();
        e.deliver_reviews(&runtime, std::slice::from_ref(&pr));
        assert_eq!(
            rest.asked.borrow().len(),
            3,
            "a retarget is not news: {:?}",
            rest.asked.borrow()
        );
        let state = state_of(&e);
        assert_eq!(state.updated_at, "2026-07-28T12:00:00Z");
        assert_eq!(state.base_ref, "main");
        assert!(runtime.prompts.borrow().is_empty());
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
    /// a `review` row like any other, and there is no one to tell — but the
    /// row itself is still owed the verdict (gh#409): the fetch that computes
    /// it is not delivery, and gating it on a chat left an undispatched row
    /// unable to acquire a verdict at all.
    #[test]
    fn an_undispatched_pull_request_tells_nobody_and_still_acquires_the_verdict() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(feedback_fixture()));
        let e = engine(rest.clone());
        let pr = pull("someone/elses-branch");
        e.db.upsert_task(&pr.to_upsert()).unwrap();
        e.db.set_pr(&pr.task_id(), Some(&pr.url), Some(pr.number), true)
            .unwrap();
        e.rederive_all().unwrap();

        let runtime = FakeRuntime::holding("/wt/gh-13-1");
        e.deliver_reviews(&runtime, std::slice::from_ref(&pr));

        assert_eq!(
            rest.asked.borrow().len(),
            3,
            "all three endpoints are read: {:?}",
            rest.asked.borrow()
        );
        assert!(
            runtime.prompts.borrow().is_empty(),
            "and nobody is prompted: {:?}",
            runtime.prompts.borrow()
        );
        let row = e.db.get_task(&pr.task_id()).unwrap().unwrap();
        assert_eq!(
            row.pr_changes_requested,
            Some(900),
            "the row carries the standing verdict",
        );
        assert!(
            !load(&e.db, &pr.task_id()).noted_gone,
            "an undispatched row is not a lost chat, and is not mourned as one",
        );

        // The steady state is as cheap as ever: nothing moved, nothing asked.
        e.deliver_reviews(&runtime, &[pr]);
        assert_eq!(rest.asked.borrow().len(), 3, "the updated_at gate held");
    }

    /// The measured failure (gh#409): a stack of undispatched pull requests —
    /// the rows gh#344 deliberately made reviewable — with a real `changes
    /// requested` on the bottom layer, and 27 poll cycles in which nothing
    /// moved. The layer above must leave `review` in the same cycle, exactly
    /// as it does when the stack was dispatched: a diff about to be rebased is
    /// not reviewable in good faith, and there is no chat anywhere in this
    /// stack to carry the fact instead of the rows.
    #[test]
    fn changes_requested_on_an_undispatched_layer_moves_the_stack_above_it() {
        // The bottom layer's endpoints carry the review; the top layer is
        // fetched too now — it is a review row with an open pull request —
        // and has nothing to say.
        let mut routes = feedback_fixture();
        routes.extend([
            (
                "/repos/o/r/issues/15/comments".to_string(),
                serde_json::json!([]),
            ),
            (
                "/repos/o/r/pulls/15/comments".to_string(),
                serde_json::json!([]),
            ),
            (
                "/repos/o/r/pulls/15/reviews".to_string(),
                serde_json::json!([]),
            ),
        ]);
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(routes));
        let e = engine(rest.clone());

        // Two undispatched pull requests GitHub reports as one stack.
        let bottom = pull("stack/layer-1");
        let mut top = pull("stack/layer-2");
        top.number = 15;
        top.url = "https://github.com/o/r/pull/15".into();
        top.base_ref = "stack/layer-1".into();
        for (pr, position) in [(&bottom, 1), (&top, 2)] {
            e.db.upsert_task(&pr.to_upsert()).unwrap();
            e.db.set_pr(&pr.task_id(), Some(&pr.url), Some(pr.number), true)
                .unwrap();
            e.db.set_pr_topology(
                &pr.task_id(),
                Some(&pr.base_ref),
                Some(&pr.head_ref),
                Some(&crate::model::PrStack {
                    number: 7,
                    size: Some(2),
                    position: Some(position),
                    base_ref: Some("main".into()),
                }),
            )
            .unwrap();
        }
        e.rederive_all().unwrap();
        assert_eq!(
            e.db.get_task("gh:o/r!15").unwrap().unwrap().state,
            BoardState::Review,
        );

        let runtime = FakeRuntime::holding("/nowhere");
        e.deliver_reviews(&runtime, &[bottom, top]);

        assert!(
            runtime.prompts.borrow().is_empty(),
            "no chat anywhere in this stack: {:?}",
            runtime.prompts.borrow()
        );
        assert_eq!(
            e.db.get_task("gh:o/r!14")
                .unwrap()
                .unwrap()
                .pr_changes_requested,
            Some(900),
            "the reviewed layer's row carries the verdict",
        );
        assert_eq!(
            e.db.get_task("gh:o/r!15").unwrap().unwrap().state,
            BoardState::Blocked,
            "and the layer above left review in the same cycle",
        );
        // The row says why, in the words both viewports already render.
        let rows = crate::rows::board_rows(&e.db, &e.cfg).unwrap();
        let above = rows.iter().find(|r| r.id == "gh:o/r!15").unwrap();
        assert_eq!(above.changes_below, Some(14));
        assert_eq!(above.landing.as_deref(), Some("changes-below"));
    }

    // ---- the fan-out down the stack (§gh#289) ----------------------------

    /// gh#20's pull request, stacked on gh#13's branch: layer 2 of the pair the
    /// tests below fan out along.
    pub(crate) fn child_pull() -> PullRequest {
        PullRequest {
            number: 15,
            url: "https://github.com/o/r/pull/15".into(),
            title: "Emit what the parser reads".into(),
            base_ref: "board/gh-13".into(),
            ..pull("board/gh-20")
        }
    }

    /// gh#21's pull request, stacked on gh#20: the third layer that proves only
    /// the direct child of a changed layer owns the ordered upstack rebase.
    fn grandchild_pull() -> PullRequest {
        PullRequest {
            number: 16,
            url: "https://github.com/o/r/pull/16".into(),
            title: "Render the emitted groups".into(),
            base_ref: "board/gh-20".into(),
            ..pull("board/gh-21")
        }
    }

    /// A second dispatched row whose branch was cut from gh#13's attempt — the
    /// edge GitHub is never told about (gh#285), and the one this fan-out has to
    /// work along whether or not there is a `stack` object anywhere.
    pub(crate) fn seed_stacked_child(e: &SyncEngine, chat: &str) {
        let parent = e.db.attempts_for("gh:o/r#13").unwrap()[0].id;
        e.db.upsert_task(&crate::db::UpsertTask {
            id: "gh:o/r#20".into(),
            source: Source::Github,
            source_id: "n20".into(),
            identifier: "gh#20".into(),
            title: "Emit what the parser reads".into(),
            body: None,
            url: "https://github.com/o/r/issues/20".into(),
            labels: vec![],
            source_state: Some("open".into()),
            upstream: UpstreamState::Unstarted,
            updated_at: crate::db::now(),
        })
        .unwrap();
        let a =
            e.db.insert_attempt(&crate::db::NewAttempt {
                automation: None,
                automation_owner: None,
                stacked_on: Some(parent),
                task_id: "gh:o/r#20".into(),
                pane_id: None,
                workspace: "offhand".into(),
                runtime: "claude-code".into(),
                worktree: Some("/wt/gh-20-1".into()),
                branch: Some("board/gh-20".into()),
                dispatched_by: None,
                dispatched_by_pane: None,
                base_sha: None,
                account: None,
                repo_path: None,
                dispatched_by_device: None,
                dispatched_by_user: None,
                dispatched_by_verified: false,
                billed_to: None,
            })
            .unwrap();
        e.db.set_attempt_pane(a, chat).unwrap();
        e.db.close_attempt(a, Outcome::Done).unwrap();
        e.db.set_pr(
            "gh:o/r#20",
            Some("https://github.com/o/r/pull/15"),
            Some(15),
            true,
        )
        .unwrap();
        e.rederive_all().unwrap();
        assert_eq!(
            e.db.get_task("gh:o/r#20").unwrap().unwrap().state,
            BoardState::Review,
        );
    }

    fn seed_stacked_grandchild(e: &SyncEngine, chat: &str) {
        let parent = e.db.attempts_for("gh:o/r#20").unwrap()[0].id;
        e.db.upsert_task(&crate::db::UpsertTask {
            id: "gh:o/r#21".into(),
            source: Source::Github,
            source_id: "n21".into(),
            identifier: "gh#21".into(),
            title: "Render the emitted groups".into(),
            body: None,
            url: "https://github.com/o/r/issues/21".into(),
            labels: vec![],
            source_state: Some("open".into()),
            upstream: UpstreamState::Unstarted,
            updated_at: crate::db::now(),
        })
        .unwrap();
        let a =
            e.db.insert_attempt(&crate::db::NewAttempt {
                automation: None,
                automation_owner: None,
                stacked_on: Some(parent),
                task_id: "gh:o/r#21".into(),
                pane_id: None,
                workspace: "offhand".into(),
                runtime: "claude-code".into(),
                worktree: Some("/wt/gh-21-1".into()),
                branch: Some("board/gh-21".into()),
                dispatched_by: None,
                dispatched_by_pane: None,
                base_sha: None,
                account: None,
                repo_path: None,
                dispatched_by_device: None,
                dispatched_by_user: None,
                dispatched_by_verified: false,
                billed_to: None,
            })
            .unwrap();
        e.db.set_attempt_pane(a, chat).unwrap();
        e.db.close_attempt(a, Outcome::Done).unwrap();
        e.db.set_pr(
            "gh:o/r#21",
            Some("https://github.com/o/r/pull/16"),
            Some(16),
            true,
        )
        .unwrap();
        e.rederive_all().unwrap();
        assert_eq!(
            e.db.get_task("gh:o/r#21").unwrap().unwrap().state,
            BoardState::Review,
        );
    }

    /// Both layers of the pair, as the sync cycle hands them over.
    pub(crate) fn stacked_pulls() -> Vec<PullRequest> {
        vec![pull("board/gh-13"), child_pull()]
    }

    pub(crate) fn runtime_for_the_pair() -> FakeRuntime {
        FakeRuntime::holding_each(&[("chat-1", "/wt/gh-13-1"), ("chat-2", "/wt/gh-20-1")])
    }

    /// The headline. `changes requested` on the lower layer reaches the agent
    /// that wrote it *and* the agent standing on top of it — the second one with
    /// a notice rather than the review, because the review is not its to answer.
    #[test]
    fn changes_requested_on_a_layer_reaches_the_layer_above_it() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(feedback_fixture()));
        let e = engine(rest.clone());
        seed_reviewed_task(&e, "board/gh-13", "chat-1");
        seed_stacked_child(&e, "chat-2");
        let runtime = runtime_for_the_pair();

        e.deliver_reviews(&runtime, &stacked_pulls());

        // The author got the review, as it always did.
        let review = runtime.said_to("chat-1").expect("the author was told");
        assert!(
            review.contains("Split the watermark per endpoint."),
            "{review}"
        );

        // The layer above got a notice: what happened, where, and what it does
        // to this branch — and not one word of the review body.
        let notice = runtime.said_to("chat-2").expect("the layer above was told");
        assert!(
            notice.contains("the layer below yours was asked to change"),
            "{notice}"
        );
        assert!(notice.contains("gh#13"), "it names the row below: {notice}");
        assert!(
            notice.contains("board/gh-13"),
            "and the branch that will move: {notice}"
        );
        assert!(
            notice.contains("PR #15"),
            "and this agent's own pull request: {notice}"
        );
        assert!(
            notice.contains("gh stack rebase --upstack"),
            "the child has to replay its own branch after the fix is pushed: {notice}",
        );
        assert!(
            !notice.contains("the replay is GitHub's to do"),
            "GitHub only replays an upper branch when the lower layer merges: {notice}",
        );
        assert!(
            !notice.contains("Split the watermark per endpoint."),
            "the review below is not this agent's to answer: {notice}"
        );

        // Recorded on the layer that was reviewed, per edge.
        let state = state_of(&e);
        assert_eq!(state.changes_requested, Some(900));
        assert_eq!(state.fanned_out.get("gh:o/r#20"), Some(&900));
        // …and standing on its row, which is what the layers above derive from.
        let below = e.db.get_task("gh:o/r#13").unwrap().unwrap();
        assert_eq!(below.pr_changes_requested, Some(900));
        assert_eq!(
            e.db.get_task("gh:o/r#20")
                .unwrap()
                .unwrap()
                .pr_changes_requested,
            None,
            "the fact belongs to the layer that was asked, not to its dependents",
        );
    }

    /// In A ← B ← C, B owns the one ordered replay that carries both B and C
    /// onto A's fix. C still hears why its diff is held, but a second actionable
    /// command there would race B or try to replay C onto stale B.
    #[test]
    fn only_the_direct_child_owns_the_ordered_upstack_rebase() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(feedback_fixture()));
        let e = engine(rest.clone());
        seed_reviewed_task(&e, "board/gh-13", "chat-1");
        seed_stacked_child(&e, "chat-2");
        seed_stacked_grandchild(&e, "chat-3");
        let runtime = FakeRuntime::holding_each(&[
            ("chat-1", "/wt/gh-13-1"),
            ("chat-2", "/wt/gh-20-1"),
            ("chat-3", "/wt/gh-21-1"),
        ]);

        e.deliver_reviews(
            &runtime,
            &[pull("board/gh-13"), child_pull(), grandchild_pull()],
        );

        let owner = runtime
            .said_to("chat-2")
            .expect("the direct child was told");
        let farther = runtime
            .said_to("chat-3")
            .expect("the farther dependent was told");
        assert!(
            owner.contains("gh stack rebase --upstack"),
            "the direct child owns the ordered replay: {owner}",
        );
        assert!(
            farther.contains("direct child") && farther.contains("owns the ordered replay"),
            "the farther layer knows which part of the stack will carry it: {farther}",
        );
        assert!(
            !farther.contains("gh stack rebase --upstack"),
            "the farther layer must not race or repeat the owner's replay: {farther}",
        );
        assert_eq!(
            runtime
                .prompts
                .borrow()
                .iter()
                .filter(|(_, text)| text.contains("gh stack rebase --upstack"))
                .count(),
            1,
            "one changed stack path has exactly one actionable rebase owner",
        );
        let state = state_of(&e);
        assert_eq!(state.fanned_out.get("gh:o/r#20"), Some(&900));
        assert_eq!(state.fanned_out.get("gh:o/r#21"), Some(&900));
    }

    /// The `hold` half of the answer: the layer above leaves the review section
    /// in the same cycle, because a diff that is about to be replayed is not
    /// reviewable in good faith — and this pass runs after the cycle's own
    /// derivation, so it has to redo it.
    #[test]
    fn the_layer_above_comes_out_of_review_in_the_same_cycle() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(feedback_fixture()));
        let e = engine(rest.clone());
        seed_reviewed_task(&e, "board/gh-13", "chat-1");
        seed_stacked_child(&e, "chat-2");

        e.deliver_reviews(&runtime_for_the_pair(), &stacked_pulls());

        assert_eq!(
            e.db.get_task("gh:o/r#20").unwrap().unwrap().state,
            BoardState::Blocked,
            "the layer above is waiting on the one below, not waiting on a human",
        );
        assert_eq!(
            e.db.get_task("gh:o/r#13").unwrap().unwrap().state,
            BoardState::Review,
            "the layer that was reviewed is exactly where it was",
        );

        // And the row says why, in the words both viewports already render.
        let rows = crate::rows::board_rows(&e.db, &e.cfg).unwrap();
        let child = rows.iter().find(|r| r.id == "gh:o/r#20").unwrap();
        assert_eq!(child.changes_below, Some(14));
        assert_eq!(child.landing.as_deref(), Some("changes-below"));
        assert_eq!(
            comet_proto::view::board::landing_note(child).as_deref(),
            Some("PR #14 below was asked to change · this rebases under it"),
        );
    }

    /// The `release` half, the way gh#411 met it on a real stack: the standing
    /// review is *dismissed* — rewritten in place, so the fetch returns the same
    /// id below the watermark with nothing appended — and the layer above comes
    /// back into review in the same cycle. Before the fix the only exits from
    /// the derived `blocked` were an approval and the pull request closing, and
    /// a bot reviewer is refused the first.
    #[test]
    fn a_dismissed_review_unblocks_the_layer_above_in_the_same_cycle() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(vec![
            (
                "/repos/o/r/issues/14/comments".into(),
                serde_json::json!([]),
            ),
            ("/repos/o/r/pulls/14/comments".into(), serde_json::json!([])),
            (
                "/repos/o/r/pulls/14/reviews".into(),
                serde_json::json!([{ "id": 900, "state": "DISMISSED",
                                     "body": "Split the watermark per endpoint.",
                                     "submitted_at": "2999-01-01T00:00:00Z",
                                     "user": { "login": "b" } }]),
            ),
        ]));
        let e = engine(rest.clone());
        seed_reviewed_task(&e, "board/gh-13", "chat-1");
        seed_stacked_child(&e, "chat-2");

        // An earlier cycle delivered review 900, recorded the request and told
        // the layer above; the child has been sitting in `blocked` since.
        let mut state = seen("2026-07-28T11:00:00Z");
        state.review = 900;
        state.changes_requested = Some(900);
        state.fanned_out.insert("gh:o/r#20".into(), 900);
        store(&e.db, "gh:o/r#13", &state).unwrap();
        e.rederive_all().unwrap();
        assert_eq!(
            e.db.get_task("gh:o/r#20").unwrap().unwrap().state,
            BoardState::Blocked,
        );

        let runtime = runtime_for_the_pair();
        e.deliver_reviews(&runtime, &stacked_pulls());

        assert_eq!(
            state_of(&e).changes_requested,
            None,
            "the dismissal ends the objection",
        );
        assert!(
            runtime.prompts.borrow().is_empty(),
            "and the withdrawn review interrupts nobody: {:?}",
            runtime.prompts.borrow(),
        );
        assert_eq!(
            e.db.get_task("gh:o/r#20").unwrap().unwrap().state,
            BoardState::Review,
            "the layer above is reviewable again without waiting for a merge",
        );
    }

    /// One review, one notice per layer. The watermark is what makes the second
    /// cycle silent — and the `updated_at` gate means it costs no call either.
    #[test]
    fn one_review_fans_out_once() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(feedback_fixture()));
        let e = engine(rest.clone());
        seed_reviewed_task(&e, "board/gh-13", "chat-1");
        seed_stacked_child(&e, "chat-2");
        let runtime = runtime_for_the_pair();

        e.deliver_reviews(&runtime, &stacked_pulls());
        assert_eq!(runtime.prompts.borrow().len(), 2);
        let asked = rest.asked.borrow().len();

        e.deliver_reviews(&runtime, &stacked_pulls());
        assert_eq!(
            runtime.prompts.borrow().len(),
            2,
            "the same review must not arrive twice anywhere: {:?}",
            runtime
                .prompts
                .borrow()
                .iter()
                .map(|(c, _)| c.clone())
                .collect::<Vec<_>>(),
        );
        assert_eq!(rest.asked.borrow().len(), asked, "and asks nothing again");
    }

    /// A dependent whose chat has been reaped is not a dependent nobody is told
    /// about. There is no agent to inform; the human about to review that diff is
    /// still there, and the row is the surface they are looking at.
    #[test]
    fn a_dependent_with_no_chat_left_is_told_on_its_row() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(feedback_fixture()));
        let e = engine(rest.clone());
        seed_reviewed_task(&e, "board/gh-13", "chat-1");
        seed_stacked_child(&e, "chat-2");
        let mut runtime = runtime_for_the_pair();
        runtime.reaped.insert("chat-2".into());

        e.deliver_reviews(&runtime, &stacked_pulls());

        assert!(runtime.said_to("chat-2").is_none(), "there is nobody there");
        assert_eq!(
            e.db.get_task("gh:o/r#20").unwrap().unwrap().state,
            BoardState::Blocked,
            "and the row carries it anyway — silence here is the original bug, \
             one layer down",
        );
        // Recorded as told, so it is not reconsidered every thirty seconds for
        // the rest of the pull request's life.
        assert_eq!(state_of(&e).fanned_out.get("gh:o/r#20"), Some(&900));
    }

    /// A dependent the *ledger* refused is a different case: that is a failure
    /// with a retry in it, so nothing is recorded and the next cycle tries again.
    #[test]
    fn a_notice_the_ledger_refused_is_retried_rather_than_recorded() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(feedback_fixture()));
        let e = engine(rest.clone());
        seed_reviewed_task(&e, "board/gh-13", "chat-1");
        seed_stacked_child(&e, "chat-2");
        let mut runtime = runtime_for_the_pair();
        runtime.prompt_fails = true;

        e.deliver_reviews(&runtime, &stacked_pulls());
        assert!(state_of(&e).fanned_out.is_empty());

        // The author's own delivery failed too, so the review is refetched and
        // both halves are tried again.
        runtime.prompt_fails = false;
        e.deliver_reviews(&runtime, &stacked_pulls());
        assert!(runtime.said_to("chat-2").is_some(), "the retry told it");
        assert_eq!(state_of(&e).fanned_out.get("gh:o/r#20"), Some(&900));
    }

    /// Direction. A review of the *upper* layer says nothing about the layer
    /// holding it up: that one is still correct, still mergeable, still
    /// reviewable, and telling its agent would be noise it cannot act on.
    #[test]
    fn nothing_fans_downwards() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(vec![
            (
                "/repos/o/r/issues/15/comments".into(),
                serde_json::json!([]),
            ),
            ("/repos/o/r/pulls/15/comments".into(), serde_json::json!([])),
            (
                "/repos/o/r/pulls/15/reviews".into(),
                serde_json::json!([{ "id": 900, "state": "CHANGES_REQUESTED",
                                     "body": "The emitter drops empty groups.",
                                     "submitted_at": "2999-01-01T00:00:00Z",
                                     "user": { "login": "b" } }]),
            ),
            (
                "/repos/o/r/issues/14/comments".into(),
                serde_json::json!([]),
            ),
            ("/repos/o/r/pulls/14/comments".into(), serde_json::json!([])),
            ("/repos/o/r/pulls/14/reviews".into(), serde_json::json!([])),
        ]));
        let e = engine(rest.clone());
        seed_reviewed_task(&e, "board/gh-13", "chat-1");
        seed_stacked_child(&e, "chat-2");
        let runtime = runtime_for_the_pair();

        e.deliver_reviews(&runtime, &stacked_pulls());

        assert!(
            runtime.said_to("chat-2").unwrap().contains("empty groups"),
            "the reviewed layer's own author is told, as ever",
        );
        assert!(
            runtime.said_to("chat-1").is_none(),
            "and the layer below hears nothing: nothing about it changed",
        );
        assert_eq!(
            e.db.get_task("gh:o/r#13").unwrap().unwrap().state,
            BoardState::Review,
        );
    }

    /// An approval fans out to nobody. An N-layer stack would generate O(N²)
    /// notices over its life, every one of them "somebody below you is fine" and
    /// none of them actionable.
    #[test]
    fn an_approval_does_not_fan_out() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(vec![
            (
                "/repos/o/r/issues/14/comments".into(),
                serde_json::json!([]),
            ),
            ("/repos/o/r/pulls/14/comments".into(), serde_json::json!([])),
            (
                "/repos/o/r/pulls/14/reviews".into(),
                serde_json::json!([{ "id": 900, "state": "APPROVED",
                                     "body": "Good — merge it.",
                                     "submitted_at": "2999-01-01T00:00:00Z",
                                     "user": { "login": "b" } }]),
            ),
        ]));
        let e = engine(rest.clone());
        seed_reviewed_task(&e, "board/gh-13", "chat-1");
        seed_stacked_child(&e, "chat-2");
        let runtime = runtime_for_the_pair();

        e.deliver_reviews(&runtime, &stacked_pulls());

        assert!(runtime.said_to("chat-1").is_some(), "the author hears it");
        assert!(runtime.said_to("chat-2").is_none(), "and nobody else does");
        assert_eq!(state_of(&e).changes_requested, None);
        assert_eq!(
            e.db.get_task("gh:o/r#20").unwrap().unwrap().state,
            BoardState::Review,
            "the layer above is still reviewable",
        );
    }

    /// The verdict is the *last* one, and only the two that mean anything count.
    /// An approval withdraws the objection — including one with nothing written
    /// in it, which interrupts nobody and still ends the wait — and a comment
    /// leaves it exactly where it was.
    #[test]
    fn an_approval_withdraws_a_standing_request_and_a_comment_does_not() {
        let asked = verdict(
            900,
            "changes_requested",
            "Split the watermark.",
            "2026-07-28T11:30:00Z",
        );
        let mut state = seen("2026-07-28T11:00:00Z");
        plan(&mut state, "2026-07-28T11:30:00Z", vec![asked.clone()]);
        assert_eq!(state.changes_requested, Some(900));

        // A `commented` review says nothing about whether the objection stands.
        let mut standing = state.clone();
        plan(
            &mut standing,
            "2026-07-28T11:40:00Z",
            vec![verdict(
                901,
                "commented",
                "One more thought.",
                "2026-07-28T11:40:00Z",
            )],
        );
        assert_eq!(standing.changes_requested, Some(900));

        // Nor does an ordinary comment, or a batch with no verdict in it at all.
        let mut standing = state.clone();
        plan(
            &mut standing,
            "2026-07-28T11:40:00Z",
            vec![feedback(
                FeedbackKind::Issue,
                42,
                "Fixed in abc123.",
                "2026-07-28T11:40:00Z",
            )],
        );
        assert_eq!(standing.changes_requested, Some(900));

        // An approval does, prose or no prose.
        let mut withdrawn = state.clone();
        plan(
            &mut withdrawn,
            "2026-07-28T11:50:00Z",
            vec![verdict(902, "approved", "", "2026-07-28T11:50:00Z")],
        );
        assert_eq!(withdrawn.changes_requested, None);
    }

    /// So does dismissing the review (gh#411) — the only withdrawal a bot
    /// reviewer has, since Actions is refused `APPROVE` outright. GitHub
    /// rewrites the review's own state in place, so what the next fetch returns
    /// is the *same* review, now `dismissed`, and nothing new at all: the batch
    /// must read as "the objection is over", not as "nothing was said".
    #[test]
    fn a_dismissed_review_withdraws_a_standing_request() {
        let asked = verdict(
            900,
            "changes_requested",
            "Split the watermark.",
            "2026-07-28T11:30:00Z",
        );
        let mut state = seen("2026-07-28T11:00:00Z");
        plan(&mut state, "2026-07-28T11:30:00Z", vec![asked]);
        assert_eq!(state.changes_requested, Some(900));

        let d = plan(
            &mut state,
            "2026-07-28T11:50:00Z",
            vec![verdict(
                900,
                "dismissed",
                "Split the watermark.",
                "2026-07-28T11:30:00Z",
            )],
        );
        assert_eq!(state.changes_requested, None, "the objection is over");
        assert_eq!(
            d,
            Decision::NothingNew,
            "and the withdrawn prose interrupts nobody",
        );

        // A dismissal of an *older* review does not swallow a request written
        // after it: the last verdict in the batch is still the one that stands.
        let mut state = seen("2026-07-28T11:00:00Z");
        plan(
            &mut state,
            "2026-07-28T12:00:00Z",
            vec![
                verdict(900, "dismissed", "Too strict.", "2026-07-28T11:30:00Z"),
                verdict(
                    903,
                    "changes_requested",
                    "Still: split it.",
                    "2026-07-28T11:55:00Z",
                ),
            ],
        );
        assert_eq!(state.changes_requested, Some(903));
    }

    /// The verdict is not a message, so the two filters delivery applies have no
    /// business in it: a `changes requested` older than the attempt's end is below
    /// the first-sight floor and still outstanding.
    #[test]
    fn a_request_below_the_first_sight_floor_is_still_outstanding() {
        let mut state = Delivered::default();
        let d = plan(
            &mut state,
            "2026-07-28T12:00:00Z",
            vec![verdict(
                900,
                "changes_requested",
                "Not quite.",
                "2026-07-28T09:30:00Z",
            )],
        );
        assert_eq!(d, Decision::NothingNew, "nobody is handed a back catalogue");
        assert_eq!(
            state.changes_requested,
            Some(900),
            "and the layers above are still standing on a branch that will move",
        );
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
