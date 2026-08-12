//! The outbound half of review delivery (§gh#239): a verdict written in the
//! review window, posted to GitHub **and** handed to the agent that is still
//! standing in the checkout it is about.
//!
//! [`crate::review`] is the inbound half — GitHub review comments noticed by
//! the sync loop and relayed into the authoring chat. This is what happens when
//! the review is written *here*. Three things make that different from typing
//! the same sentence into GitHub:
//!
//! 1. **The unclaimed changes ride along.** The reviewer does not retype them.
//!    They are the remainder [`crate::claims`] derives — diff minus claim
//!    anchors — recomputed at submit time and attached to both copies, so the
//!    agent is told what nobody accounted for in the same breath as the verdict.
//! 2. **It goes into the checkout the agent is already in.** The chat that
//!    wrote the pull request is still open with the whole task in context, so
//!    the fix is a commit on the same branch rather than a new dispatch and a
//!    new worktree.
//! 3. **Once.** A verdict delivered twice is worse than one delivered late, and
//!    there are two ways to send it twice: submitting twice, and having the
//!    sync loop find our own review on GitHub and relay it back. Both are
//!    closed with the watermarks §gh#180 already keeps ([`Delivered`]), not
//!    with a second bookkeeping scheme:
//!
//!    - the submission is recorded under its [`fingerprint`], so a retry
//!      finishes the half that failed instead of posting a second review;
//!    - the id GitHub assigns the posted review is written straight into
//!      [`Delivered::review`], so the inbound path can never hand it back —
//!      an id at or below the watermark is consumed, forever.
//!
//! The second of those needs an id, and a GitHub that answers without one would
//! leave the relay open. So every verdict posted from here also carries
//! [`POSTED_MARK`] in its body, which [`crate::review::is_the_boards_own`]
//! recognises. The watermark is the mechanism; the mark is the backstop.
//!
//! ## A verdict is a board fact; GitHub is a projection of it (gh#365)
//!
//! Almost nothing a verdict does is GitHub's. It clears the standing objection,
//! it takes the layers stacked above this one back out of waiting, it reaches
//! the agent still sitting in the checkout — and it is the sentence a human just
//! typed. Posting it on the pull request is a *copy* of that fact, kept for the
//! people reading the thread rather than the board.
//!
//! So the order is **record, deliver, project**: the [`Submission`], the
//! standing verdict and the delivery all land before GitHub is asked anything,
//! and what GitHub says is written back onto the submission as a
//! [`Projection`]. A refusal is then a visible *unposted* verdict rather than a
//! lost one — the reviewer's words are in the agent's chat and in the ledger
//! either way, and a retry of the same submission re-tries only the projection.
//!
//! GitHub refuses one thing predictably: an App may not approve or request
//! changes on a pull request its own App opened, which is every pull request the
//! board dispatches (gh#338 hypothesis 1). That one is answered rather than
//! reported — the verdict goes out as a `COMMENT` whose first line says in words
//! that it is an approval ([`as_comment_body`]), and the submission is marked
//! [`Projection::PostedAsComment`]. Honest, needs no new credential, and leaves
//! the thread readable by people who are not looking at comet. The real fix is
//! a verdict that carries the *human's* GitHub identity, which is its own issue.
//!
//! ## What it refuses
//!
//! A closed pull request (the payload's promise that the branch is still live
//! would be a lie), a `comment` or `changes requested` with no prose (GitHub
//! refuses those itself, and a verdict that is only a verdict is not one), and
//! a chat that has moved: `chat_alive` plus [`crate::review::
//! still_the_authors_checkout`], the same author check the inbound path makes,
//! because pasting a review into a session that has moved on to other work
//! delivers it to the wrong author. The verdict still stands and is still
//! projected — the pull request is where it belongs regardless — and the receipt
//! says nobody was told.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::claims::{AttemptReview, ChangedFile};
use crate::model::{Attempt, Task};
use crate::review::{Delivered, still_the_authors_checkout};
use crate::runtime::Runtime;
use crate::sources::github::{Github, Rest, refused_own_pull_request};
use crate::sync::SyncEngine;

/// The trailer every verdict posted through the board carries.
///
/// True of every one of them and of nothing else on a pull request, which is
/// what makes it usable as the backstop for a GitHub reply that carried no id.
/// It says where the verdict was written and nothing about where else it
/// reached — a reader on the pull request cannot check that, and the receipt is
/// where the board answers it.
pub const POSTED_MARK: &str = "— written in comet-board's review window.";

/// How many submissions one task remembers. A bound on a `meta` blob, not a
/// ration: falling off the end costs the oldest verdict its retry protection,
/// long after anybody would retry it.
pub const MAX_SUBMISSIONS: usize = 20;

/// What the reviewer decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictKind {
    Comment,
    Approve,
    ChangesRequested,
}

impl VerdictKind {
    /// Read a verdict off the wire. `request_changes` is accepted beside
    /// `changes_requested` because GitHub spells the event one way and the
    /// review state the other, and a caller that mixes them up is not wrong
    /// about what it means.
    pub fn parse(s: &str) -> Option<VerdictKind> {
        match s.trim() {
            "comment" | "commented" => Some(VerdictKind::Comment),
            "approve" | "approved" => Some(VerdictKind::Approve),
            "changes_requested" | "request_changes" => Some(VerdictKind::ChangesRequested),
            _ => None,
        }
    }

    /// The wire spelling — GitHub's review *state*, which is also what the
    /// inbound path reads back off a review it did not write.
    pub fn as_str(self) -> &'static str {
        match self {
            VerdictKind::Comment => "commented",
            VerdictKind::Approve => "approved",
            VerdictKind::ChangesRequested => "changes_requested",
        }
    }

    /// The `event` GitHub's review endpoint takes.
    pub fn event(self) -> &'static str {
        match self {
            VerdictKind::Comment => "COMMENT",
            VerdictKind::Approve => "APPROVE",
            VerdictKind::ChangesRequested => "REQUEST_CHANGES",
        }
    }

    /// How the payload names it, in the spelling the inbound path already uses
    /// for a review it read: `[review · changes requested]`.
    pub fn label(self) -> &'static str {
        match self {
            VerdictKind::Comment => "commented",
            VerdictKind::Approve => "approved",
            VerdictKind::ChangesRequested => "changes requested",
        }
    }

    /// Does this verdict have to carry prose? GitHub refuses a `COMMENT` or a
    /// `REQUEST_CHANGES` with an empty body, and it is right to: a verdict with
    /// nothing in it tells the agent to change something unnamed.
    pub fn needs_comment(self) -> bool {
        !matches!(self, VerdictKind::Approve)
    }

    /// The verdict in the present tense — what a comment carrying it says it
    /// is, for a reader on the pull request rather than on the board (gh#365).
    pub fn says(self) -> &'static str {
        match self {
            VerdictKind::Comment => "comments",
            VerdictKind::Approve => "approves",
            VerdictKind::ChangesRequested => "requests changes",
        }
    }

    /// The same thing as a sentence, which is how a downgraded verdict opens.
    pub fn heading(self) -> &'static str {
        match self {
            VerdictKind::Comment => "This is a comment.",
            VerdictKind::Approve => "This is an approval.",
            VerdictKind::ChangesRequested => "This asks for changes.",
        }
    }

    /// The verb GitHub's refusal is about, as it reads in "GitHub does not let
    /// the board _ its own pull request".
    pub fn refused_verb(self) -> &'static str {
        match self {
            VerdictKind::Comment => "comment on",
            VerdictKind::Approve => "approve",
            VerdictKind::ChangesRequested => "request changes on",
        }
    }
}

/// Where a verdict's copy on the pull request got to (gh#365).
///
/// Nothing here is about whether the verdict *stands* — it always does, from
/// the moment it is recorded. This is only about the projection: whether GitHub
/// took it, took it wearing something else, or would not take it at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Projection {
    /// On the pull request, as the verdict it is.
    ///
    /// The default, and deliberately: every submission recorded before this
    /// field existed was written only after a post GitHub had accepted.
    #[default]
    Posted,
    /// On the pull request as a `COMMENT` that says in words what it is,
    /// because GitHub will not let this identity cast this verdict here.
    PostedAsComment,
    /// Not on the pull request. GitHub refused it, or could not be asked.
    Unposted,
}

impl Projection {
    /// Has GitHub got a copy of this verdict in some form? `false` is the one
    /// state a retry has work to do in.
    pub fn on_github(self) -> bool {
        !matches!(self, Projection::Unposted)
    }
}

/// One verdict this board has already submitted, keyed by what it said.
///
/// Stored inside [`Delivered`] — the same `meta` row §gh#180 keeps its
/// watermarks in — because it is the same fact about the same pull request:
/// what has already been said to this agent about this work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Submission {
    pub fingerprint: String,
    /// The review id GitHub assigned. `0` when GitHub answered without one, in
    /// which case [`POSTED_MARK`] is what keeps the relay shut — and `0` while
    /// the projection has not landed at all, because then there is no review.
    pub review_id: i64,
    /// Whether the authoring chat has been told. False after a submission whose
    /// delivery failed — one of the two halves a retry finishes.
    pub delivered: bool,
    /// Where the copy on the pull request got to. The other half a retry
    /// finishes (gh#365).
    #[serde(default)]
    pub projection: Projection,
    /// GitHub's own words for refusing to take this verdict *as* this verdict.
    /// Set for both [`Projection::PostedAsComment`] and
    /// [`Projection::Unposted`]; `None` when GitHub took it as it was sent.
    #[serde(default)]
    pub refusal: Option<String>,
}

/// What one submission did.
///
/// `snake_case` like [`AttemptReview`], and for the same reason: the CLI prints
/// both, and an object that changes case halfway through a tool's output is a
/// papercut nobody should have to remember.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerdictReceipt {
    pub task_id: String,
    pub attempt: i64,
    pub kind: VerdictKind,
    /// The GitHub review id, or `0` when GitHub did not say — or when the
    /// projection has not landed, which is a verdict that stands with no review
    /// behind it.
    pub review_id: i64,
    /// Was this the call that recorded it? `false` when a retry found the
    /// submission already on the ledger, which is what idempotency looks like
    /// from the outside. It says nothing about GitHub: that is [`Self::
    /// projection`], and a retry that recorded nothing may still have posted.
    pub recorded: bool,
    /// The chat the payload went into.
    pub chat_id: Option<String>,
    pub delivered: bool,
    /// Why nobody was told. `None` when somebody was.
    pub not_delivered: Option<String>,
    /// Where the copy on the pull request got to (gh#365).
    pub projection: Projection,
    /// GitHub's words for refusing the verdict as sent. `None` when it took it.
    pub refused: Option<String>,
    /// How many unclaimed changes rode along, on both copies.
    pub unclaimed: usize,
    /// The exact text queued into the chat — what the preview promised.
    pub payload: String,
}

/// How the payload spells one unclaimed change.
///
/// File granularity, because that is the granularity the remainder has: a
/// symbol anchor is how a *claim* is matched (§gh#235), not a way of cutting a
/// changed file into pieces. `unmentioned` on a modified file is the whole
/// point of the line — something moved inside a file nobody described.
pub fn describe(file: &ChangedFile) -> String {
    let what = match file.status.as_str() {
        "A" => format!("added, {}", file.counts()),
        "D" => format!("deleted, {}", file.counts()),
        _ => format!("{}, unmentioned", file.counts()),
    };
    format!("{} — {}", file.path, what)
}

/// The `[unclaimed]` lines, one per change no claim accounts for.
pub fn unclaimed_lines(review: &AttemptReview) -> Vec<String> {
    review
        .remainder
        .unclaimed
        .iter()
        .map(|f| format!("[unclaimed] {}", describe(f)))
        .collect()
}

/// The sentence the screen states before you submit, and the one the receipt
/// confirms after.
///
/// `delivering` is whether the payload is going into a chat at all — a bare
/// approval is not (see [`worth_delivering`]), and neither is a review whose
/// author is gone. Saying "delivered" in either case would be the screen
/// promising something it is about to not do.
pub fn contract_line(review: &AttemptReview, delivering: bool) -> String {
    if !delivering {
        return "Posted to the pull request. Nothing is delivered into the chat.".to_string();
    }
    let where_ = match review.branch.as_deref() {
        Some(branch) => branch.to_string(),
        None => "the authoring chat".to_string(),
    };
    let n = review.remainder.unclaimed.len();
    let attached = match n {
        0 => String::new(),
        1 => ", with the unclaimed change attached".to_string(),
        // The design's own phrasing for the case it draws.
        2 => ", with both unclaimed changes attached".to_string(),
        n => format!(", with all {n} unclaimed changes attached"),
    };
    format!("Delivered into {where_} once{attached}.")
}

/// The exact text the authoring chat is queued with.
///
/// Pure, and public, because the review window renders this *before* submit and
/// the board sends it *at* submit. Two functions would eventually disagree, and
/// the one screen where that must never happen is the one whose whole promise
/// is "this is what will be sent".
pub fn compose(review: &AttemptReview, kind: VerdictKind, comment: &str) -> String {
    let mut s = crate::review::reviewed_header(
        &review.brief.identifier,
        &review.brief.title,
        review.pr_url.as_deref().unwrap_or(&review.brief.url),
    );
    s.push_str(&format!("\n[review · {}]\n", kind.label()));
    let comment = comment.trim();
    if !comment.is_empty() {
        s.push_str(comment);
        s.push('\n');
    }
    let unclaimed = unclaimed_lines(review);
    if !unclaimed.is_empty() {
        s.push('\n');
        for line in unclaimed {
            s.push_str(&line);
            s.push('\n');
        }
    }
    s.push_str(&format!(
        "\n{}\n{}.\n",
        crate::review::STILL_OPEN,
        crate::review::ADDRESS_IT
    ));
    s
}

/// The body posted on the pull request.
///
/// The same verdict and the same remainder as the chat gets — "attached to
/// both" is the contract — without the chat's closing instruction, which is
/// addressed to an agent sitting in a checkout and reads as noise to everybody
/// else reading the pull request.
pub fn github_body(review: &AttemptReview, comment: &str) -> String {
    let mut s = comment.trim().to_string();
    let unclaimed = unclaimed_lines(review);
    if !unclaimed.is_empty() {
        if !s.is_empty() {
            s.push_str("\n\n");
        }
        s.push_str(&format!(
            "{} change{} no claim accounts for:\n\n",
            unclaimed.len(),
            if unclaimed.len() == 1 { "" } else { "s" }
        ));
        for line in &unclaimed {
            s.push_str(&format!(
                "- `{}`\n",
                line.trim_start_matches("[unclaimed] ")
            ));
        }
    }
    if !s.is_empty() {
        s.push('\n');
    }
    s.push_str(POSTED_MARK);
    s
}

/// The body a verdict wears when GitHub will not take it as a verdict (gh#365).
///
/// [`github_body`]'s text with a sentence in front of it saying what this
/// actually is, because a `COMMENT` that reads as an approval only to somebody
/// who knows comet's internals is not a review anybody can act on. It names the
/// reason too: an App may not approve a pull request its own App opened, and a
/// reader wondering why a bot is announcing an approval instead of casting one
/// deserves the answer in the same paragraph.
///
/// [`POSTED_MARK`] stays where [`github_body`] put it — last — so the inbound
/// path's backstop is untouched by the downgrade.
pub fn as_comment_body(kind: VerdictKind, body: &str) -> String {
    format!(
        "**{}** GitHub does not let this App {} a pull request its own App opened, \
         so it is posted as a comment. comet-board has it recorded as {}.\n\n{body}",
        kind.heading(),
        kind.refused_verb(),
        kind.label(),
    )
}

/// One sentence for where the copy on the pull request got to, written for
/// somebody who does not know GitHub's rules about who may review what.
///
/// Shared by every surface that prints a receipt, so the desktop, the CLI and
/// the phone cannot disagree about what just happened.
pub fn projection_line(kind: VerdictKind, projection: Projection, refused: Option<&str>) -> String {
    match projection {
        Projection::Posted => "It is on the pull request.".to_string(),
        Projection::PostedAsComment => format!(
            "It is on the pull request as a comment that says it {} — GitHub does not let \
             the board {} its own pull request.",
            kind.says(),
            kind.refused_verb(),
        ),
        Projection::Unposted => match refused {
            Some(why) => format!("It is not on the pull request — GitHub refused it: {why}"),
            None => "It is not on the pull request.".to_string(),
        },
    }
}

/// What one submission did, in the order the work happens (gh#365): the
/// verdict and its delivery, which stand whatever GitHub says, and then the
/// copy on the pull request.
///
/// Here rather than in the review window because three surfaces print it — the
/// desktop, the CLI and the phone — and a verdict that reads as posted on one
/// and unposted on another is the confusion this issue is about.
pub fn receipt_line(receipt: &VerdictReceipt) -> String {
    // The idempotent path says so rather than pretending it just happened.
    let stands = if receipt.recorded {
        "Recorded"
    } else {
        "Already recorded"
    };
    let board = match (receipt.delivered, receipt.not_delivered.as_deref()) {
        (true, _) => format!("{stands}, and delivered into the chat once."),
        (false, Some(why)) => format!("{stands}. Nothing was delivered into the chat: {why}."),
        (false, None) => format!("{stands}."),
    };
    format!(
        "{board} {}",
        projection_line(receipt.kind, receipt.projection, receipt.refused.as_deref())
    )
}

/// Should the review window keep what the reviewer typed after a submission?
///
/// Only when GitHub has no copy of it (gh#365). Ordinarily the box is emptied
/// for the next verdict — the words are in the agent's chat and on the pull
/// request, and leaving them there invites a click that would be refused as the
/// same submission. Unposted, they are the one copy a person can still do
/// something with: retry it, or paste it onto GitHub by hand. The retry is the
/// same submission and posts nothing twice; it finishes the projection.
pub fn keeps_the_comment(receipt: &VerdictReceipt) -> bool {
    !receipt.projection.on_github()
}

/// The id a verdict stands under when GitHub has not given it one.
///
/// [`crate::review::Delivered::changes_requested`] is a watermark as well as a
/// fact — the fan-out to the layers stacked above compares against it — so a
/// verdict recorded before it is posted still needs a number, and one above
/// everything this pull request has ever been keyed by. Real review ids are in
/// the billions and come from the same counter, so `+1` on the highest one seen
/// is below the next real id and above every id already recorded here.
fn local_verdict_id(state: &Delivered) -> i64 {
    state
        .submissions
        .iter()
        .map(|s| s.review_id)
        .chain([state.review, state.changes_requested.unwrap_or_default()])
        .max()
        .unwrap_or_default()
        + 1
}

/// Is this verdict worth interrupting the agent for?
///
/// [`crate::review::is_actionable`]'s rule, said about a verdict the board is
/// about to write rather than one it just read: changes requested is actionable
/// with or without prose, and an approval without prose says the agent has
/// nothing left to do — interrupting it to hear so is the opposite of useful.
pub fn worth_delivering(kind: VerdictKind, comment: &str) -> bool {
    kind == VerdictKind::ChangesRequested || !comment.trim().is_empty()
}

/// What identifies one submission, so a retry of it is not a second verdict.
///
/// The attempt, the verdict and the prose: a reviewer who writes a different
/// sentence has written a different review and means to send it. The same
/// sentence twice is a double click or a retried call, and is answered with the
/// receipt of the first.
///
/// FNV-1a rather than a `DefaultHasher`, because this is persisted: the std
/// hasher promises nothing about being stable across releases, and a
/// fingerprint that changes underneath a stored one silently reopens the very
/// double-post this closes.
pub fn fingerprint(attempt: i64, kind: VerdictKind, comment: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in comment.trim().as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{attempt}:{}:{hash:016x}", kind.as_str())
}

/// The pull request a verdict is about: `owner/repo` and its number.
///
/// Read off the pull request's own URL first, because that is the one field
/// that is right for a Linear task whose work landed in a GitHub repo — the
/// task id says nothing about GitHub there. Falls back to the id, which is
/// where a GitHub row's repo has always come from.
pub fn pr_target(task: &Task) -> Option<(String, i64)> {
    if let Some(url) = task.pr_url.as_deref() {
        let rest = url
            .split_once("github.com/")
            .map(|(_, rest)| rest)
            .unwrap_or(url);
        let mut parts = rest.split('/');
        if let (Some(owner), Some(repo), Some("pull"), Some(number)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
            && let Ok(number) = number.trim_end_matches('/').parse::<i64>()
            && !owner.is_empty()
            && !repo.is_empty()
        {
            return Some((format!("{owner}/{repo}"), number));
        }
    }
    let repo = crate::model::gh_repo(&task.id)?;
    Some((repo.to_string(), task.pr_number?))
}

impl SyncEngine {
    /// Submit a verdict: stand it up on the board, hand it to the agent that is
    /// still in the checkout (§gh#239), and project it onto the pull request.
    ///
    /// `attempt` names one; `None` is the task's latest, which is what a
    /// reviewer looking at a row means. The remainder is recomputed here rather
    /// than taken from the caller — a reviewer cannot be asked to retype the
    /// unclaimed set, and a caller that could supply it could also get it
    /// wrong.
    ///
    /// Order matters and is **record, deliver, project** (gh#365). Everything a
    /// verdict *is* — the standing objection or its withdrawal, the rows above
    /// it moving, the sentence reaching the agent — is the board's, and none of
    /// it is GitHub's to veto. Posting first, as this did until gh#365, made a
    /// refusal on the one write nobody else depends on throw all of them away,
    /// including the comment the reviewer had just typed. Recorded first, a
    /// refusal is an unposted [`Projection`] on a verdict that stands.
    ///
    /// The cost of the inversion is the crash window it inherits: a process
    /// that dies between the record and the post leaves a verdict GitHub has
    /// never seen. That is the recoverable direction — the submission is on the
    /// ledger with [`Projection::Unposted`] on it, and re-submitting the same
    /// words finishes the projection rather than posting a second review.
    pub fn submit_verdict(
        &self,
        runtime: Option<&dyn Runtime>,
        task_id: &str,
        attempt: Option<i64>,
        kind: VerdictKind,
        comment: &str,
    ) -> Result<VerdictReceipt> {
        let comment = comment.trim();
        if kind.needs_comment() && comment.is_empty() {
            bail!(
                "a verdict of `{}` needs something written in it — GitHub refuses an empty one, \
                 and so does an agent asked to act on it",
                kind.label()
            );
        }
        // By id or by identifier, like `claim` and `review` beside it (§gh#339):
        // the three verbs of the review contract are typed by the same hands,
        // out of the same window, and one of them refusing `gh#339` while the
        // others take it would be a distinction nobody could learn.
        let tasks = self.db.load_tasks()?;
        let task = crate::dispatch::task_by_reference(&tasks, task_id)?;
        let Some(gh) = &self.github else {
            bail!("this board has no GitHub credential, so it cannot post a review");
        };
        let (repo, number) = pr_target(task).ok_or_else(|| {
            anyhow::anyhow!(
                "{} has no pull request on it — there is nothing to review",
                task.identifier
            )
        })?;
        if !task.pr_open {
            bail!(
                "{}'s pull request is closed — the payload's promise that the branch is still \
                 live would not be true",
                task.identifier
            );
        }
        // Deliberately not gated on `[github] writeback`. That flag is off by
        // default because it governs comments the board volunteers — a
        // dispatch line on an issue nobody asked it to narrate. This is the
        // opposite kind of write: a human wrote the sentence and pressed the
        // button, in the same second, about a pull request they are looking
        // at. Refusing it would be the config answering a question it was not
        // asked.

        let review = self.review(task_id, attempt)?;
        let attempt_row = task.attempts.iter().find(|a| a.id == review.attempt);
        let payload = compose(&review, kind, comment);
        let print = fingerprint(review.attempt, kind, comment);
        let mut state = crate::review::load(&self.db, &task.id);
        let already = state
            .submissions
            .iter()
            .find(|s| s.fingerprint == print)
            .cloned();

        // ---- record ---------------------------------------------------
        // Nothing below this point can unmake the verdict. It is on the ledger,
        // the objection it clears or raises is standing, and the board has
        // already been rederived off it.
        let recorded = already.is_none();
        if recorded {
            // The standing verdict, so that a `changes requested` written here
            // reaches the layers stacked on this one through the same fan-out as
            // one noticed on GitHub (§gh#289). It cannot wait for the post: the
            // watermark consumes the board's own review the moment GitHub has
            // it, so the inbound path can never read it back and would never
            // learn that anything was asked — and if the post is refused there
            // is no review to read at all.
            state.changes_requested = match kind {
                VerdictKind::ChangesRequested => Some(local_verdict_id(&state)),
                // An approval withdraws whatever was outstanding, which is what
                // takes the rows above this one back out of waiting.
                VerdictKind::Approve => None,
                // A comment says nothing about whether an objection stands.
                VerdictKind::Comment => state.changes_requested,
            };
            state.submissions.push(Submission {
                fingerprint: print.clone(),
                review_id: 0,
                delivered: false,
                projection: Projection::Unposted,
                refusal: None,
            });
            if state.submissions.len() > MAX_SUBMISSIONS {
                let drop = state.submissions.len() - MAX_SUBMISSIONS;
                state.submissions.drain(..drop);
            }
            crate::review::store(&self.db, &task.id, &state)?;
            // A `changes requested` takes the layers stacked on this one out of
            // `review`, and an approval puts them back (§gh#289). A reviewer who
            // just pressed the button should see that on the board now rather
            // than on the next poll — and a derivation that fails is a log line,
            // not a failed submission: the verdict is already recorded.
            if let Err(e) = self.rederive_all() {
                self.log.warn(format!(
                    "rederiving after a verdict on {}: {e}",
                    task.identifier
                ));
            }
            self.log.info(format!(
                "{}: recorded a `{}` verdict on {repo}#{number}",
                task.identifier,
                kind.label()
            ));
        }

        let mut receipt = VerdictReceipt {
            task_id: task.id.clone(),
            attempt: review.attempt,
            kind,
            review_id: already.as_ref().map_or(0, |s| s.review_id),
            recorded,
            chat_id: attempt_row.and_then(|a| a.pane_id.clone()),
            delivered: already.as_ref().is_some_and(|s| s.delivered),
            not_delivered: None,
            projection: already
                .as_ref()
                .map_or(Projection::Unposted, |s| s.projection),
            refused: already.as_ref().and_then(|s| s.refusal.clone()),
            unclaimed: review.remainder.unclaimed.len(),
            payload: payload.clone(),
        };

        // ---- deliver --------------------------------------------------
        // The half gh#239 exists for, and the half GitHub has no part in: the
        // agent is still standing in the checkout this is about.
        if !receipt.delivered {
            match self.deliver_verdict(runtime, task, attempt_row, kind, comment, &payload) {
                Ok(chat_id) => {
                    receipt.delivered = true;
                    if let Some(entry) = state
                        .submissions
                        .iter_mut()
                        .find(|s| s.fingerprint == print)
                    {
                        entry.delivered = true;
                    }
                    crate::review::store(&self.db, &task.id, &state)?;
                    self.log.info(format!(
                        "{}: delivered the verdict on {repo}#{number} into chat {chat_id}",
                        task.identifier
                    ));
                }
                Err(why) => {
                    self.log.info(format!(
                        "{}: the verdict on {repo}#{number} was not delivered: {why}",
                        task.identifier
                    ));
                    receipt.not_delivered = Some(why);
                }
            }
        }

        // ---- project --------------------------------------------------
        // Last, because it is the only writer whose failure costs nothing that
        // is not already safe. A submission GitHub has a copy of is left alone,
        // which is what stops a retry posting a second review.
        if !receipt.projection.on_github() {
            // What the standing verdict is keyed by right now — the local id, if
            // this is a `changes requested` GitHub has not numbered yet. A
            // successful post moves everything keyed by it onto GitHub's number,
            // so the inbound pass reading its own review back agrees with the
            // board and the fan-out is not re-run under a second name for one
            // verdict.
            let stood_under = (kind == VerdictKind::ChangesRequested)
                .then_some(state.changes_requested)
                .flatten();
            let (projection, review_id, refused) =
                self.project_verdict(gh, &repo, number, kind, &review, comment);
            receipt.projection = projection;
            receipt.refused = refused.clone();
            if projection.on_github() {
                receipt.review_id = review_id;
                // The watermark, so an id at or below it can never come back
                // through `deliver_reviews` — this verdict arrives once, from
                // here, and not again from the poll that finds it.
                state.review = state.review.max(review_id);
                // Everything keyed by the id this verdict has been standing
                // under moves onto GitHub's number for it — both halves, or
                // one of them is wrong. The standing verdict, so the inbound
                // pass reading its own review back agrees with the board rather
                // than treating it as a second objection; and the fan-out
                // ledger, so no layer stacked above is told twice about one
                // verdict under two names.
                if let Some(local) = stood_under.filter(|l| *l != review_id && review_id > 0) {
                    state.changes_requested = Some(review_id);
                    for at in state.fanned_out.values_mut() {
                        if *at == local {
                            *at = review_id;
                        }
                    }
                }
            }
            if let Some(entry) = state
                .submissions
                .iter_mut()
                .find(|s| s.fingerprint == print)
            {
                entry.projection = projection;
                entry.review_id = review_id;
                entry.refusal = refused;
            }
            crate::review::store(&self.db, &task.id, &state)?;
            let said = format!(
                "{}: the `{}` verdict on {repo}#{number} — {}",
                task.identifier,
                kind.label(),
                projection_line(kind, receipt.projection, receipt.refused.as_deref()),
            );
            // A verdict nobody on the pull request can see is worth a louder
            // line than one that landed. It is not a failure — the verdict
            // stands and the agent has it — but somebody may want to go and
            // say it there by hand.
            match receipt.projection {
                Projection::Unposted => self.log.warn(said),
                _ => self.log.info(said),
            }
        }
        Ok(receipt)
    }

    /// Hand the payload to the agent still standing in the checkout, or say who
    /// was not told and why.
    ///
    /// `Err` is a sentence for the receipt, never a failed submission: the
    /// verdict is already recorded, and raising here would tell a reviewer
    /// nothing happened at all.
    fn deliver_verdict<'a>(
        &self,
        runtime: Option<&dyn Runtime>,
        task: &Task,
        attempt_row: Option<&'a Attempt>,
        kind: VerdictKind,
        comment: &str,
        payload: &str,
    ) -> std::result::Result<&'a str, String> {
        if !worth_delivering(kind, comment) {
            return Err("an approval with nothing to say interrupts nobody".into());
        }
        let (Some(runtime), Some(attempt_row)) = (runtime, attempt_row) else {
            return Err("there is no live runtime to deliver into".into());
        };
        let Some(chat_id) = attempt_row.pane_id.as_deref() else {
            return Err("this attempt never had a chat".into());
        };
        // The same author check the inbound path makes, and for the same
        // reason: a chat that has been re-pointed at another checkout is
        // somebody else's session, and a review pasted into it is delivered to
        // the wrong author.
        let here = runtime.chat_alive(chat_id).and_then(|alive| {
            Ok(alive
                && still_the_authors_checkout(runtime.chat_cwd(chat_id)?.as_deref(), attempt_row))
        });
        match here {
            Err(e) => return Err(format!("the chat could not be asked about: {e}")),
            Ok(false) => {
                return Err(format!(
                    "chat {chat_id} no longer holds the agent that wrote {}'s branch",
                    task.identifier
                ));
            }
            Ok(true) => {}
        }
        if let Err(e) = runtime.prompt(chat_id, payload) {
            // Recorded and undelivered is the state a retry exists for: the
            // submission is on the ledger, so the retry tries only the halves
            // that failed.
            return Err(format!("the chat could not be queued: {e}"));
        }
        Ok(chat_id)
    }

    /// Put the board's copy of the verdict on the pull request (gh#365).
    ///
    /// One refusal is answered rather than reported: GitHub will not let the
    /// App that opened a pull request approve or request changes on it, which is
    /// every pull request the board dispatched, so the verdict goes out as a
    /// `COMMENT` that says in its first line what it is. That is the difference
    /// between a review surface with one working button and one with three.
    ///
    /// Every other refusal — a body GitHub will not take, a network that is not
    /// there — comes back as [`Projection::Unposted`] with GitHub's words, for
    /// the receipt to show and a retry to try again.
    fn project_verdict(
        &self,
        gh: &Github<Box<dyn Rest>>,
        repo: &str,
        number: i64,
        kind: VerdictKind,
        review: &AttemptReview,
        comment: &str,
    ) -> (Projection, i64, Option<String>) {
        let body = github_body(review, comment);
        let refused = match gh.post_review(repo, number, kind.event(), &body) {
            Ok(id) => return (Projection::Posted, id, None),
            Err(e) => e,
        };
        // A `COMMENT` that GitHub refused is not going to be taken as a comment
        // either, so there is nothing to fall back to.
        if kind == VerdictKind::Comment || !refused_own_pull_request(&refused) {
            return (Projection::Unposted, 0, Some(format!("{refused:#}")));
        }
        let said = format!("{refused:#}");
        match gh.post_review(
            repo,
            number,
            VerdictKind::Comment.event(),
            &as_comment_body(kind, &body),
        ) {
            Ok(id) => (Projection::PostedAsComment, id, Some(said)),
            // Both attempts refused. The first refusal is the one worth
            // reporting: it is the reason the second was made.
            Err(_) => (Projection::Unposted, 0, Some(said)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claims::{Brief, Claim, DiffSource, Remainder};
    use crate::evidence::RunEvidence;

    fn changed(path: &str, status: &str, added: u32, removed: u32) -> ChangedFile {
        ChangedFile {
            path: path.into(),
            status: status.into(),
            added,
            removed,
            binary: false,
            symbols: vec![],
        }
    }

    /// The review the design draws: two claims, two changes nobody accounted
    /// for.
    fn reviewed() -> AttemptReview {
        let changed_files = vec![
            changed("crates/ui/src/shell/spaces.rs", "M", 40, 12),
            changed("Cargo.toml", "A", 1, 0),
            changed("crates/ui/src/shell.rs", "M", 6, 4),
        ];
        let remainder = crate::claims::remainder(
            &[Claim {
                text: "A live chat's row is drawn by Active and never by the spaces tree.".into(),
                files: vec!["crates/ui/src/shell/spaces.rs".into()],
                symbols: vec![],
            }],
            &changed_files,
        );
        AttemptReview {
            // gh#236 added this after this fixture was written. Default is
            // `read = false` — "not read" rather than five clean results, so
            // a fixture that does not exercise effects asserts nothing.
            effects: Default::default(),
            task_id: "gh:bredebjorhovd/comet-board#138".into(),
            attempt: 7,
            attempt_number: 1,
            state: "review".into(),
            outcome: Some("done".into()),
            branch: Some("board/gh-138".into()),
            worktree: Some("/wt/gh-138-1".into()),
            pr_url: Some("https://github.com/bredebjorhovd/comet-board/pull/212".into()),
            brief: Brief {
                identifier: "gh#138".into(),
                title: "Active owns a chat's row while its session is live".into(),
                url: "https://github.com/bredebjorhovd/comet-board/issues/138".into(),
                body: None,
            },
            claimed_at: Some("2026-08-09T10:00:00Z".into()),
            claims_error: None,
            remainder,
            changed: changed_files,
            diff: DiffSource::Checkout,
            uncommitted: Some(0),
            evidence: RunEvidence::default(),
        }
    }

    /// The payload the design specifies, line for line: who is writing, which
    /// task, the verdict, the prose, the remainder, and where to put the fix.
    #[test]
    fn the_payload_is_the_verdict_and_the_remainder_and_where_to_fix_it() {
        let review = reviewed();
        let text = compose(
            &review,
            VerdictKind::ChangesRequested,
            "The split reads right. Why does this need itertools?",
        );
        assert!(text.starts_with("comet-board: your pull request has been reviewed."));
        assert!(text.contains("gh#138 · Active owns a chat's row"), "{text}");
        assert!(
            text.contains("https://github.com/bredebjorhovd/comet-board/pull/212"),
            "{text}"
        );
        assert!(text.contains("[review · changes requested]"), "{text}");
        assert!(text.contains("Why does this need itertools?"), "{text}");
        // The remainder rides along, spelled per change and never retyped.
        assert!(
            text.contains("[unclaimed] Cargo.toml — added, +1 −0"),
            "{text}"
        );
        assert!(
            text.contains("[unclaimed] crates/ui/src/shell.rs — +6 −4, unmentioned"),
            "{text}"
        );
        // The claimed file is not in the remainder, so it is not in the payload.
        assert!(
            !text.contains("[unclaimed] crates/ui/src/shell/spaces.rs"),
            "{text}"
        );
        assert!(
            text.contains("still in the checkout you wrote it in"),
            "{text}"
        );
        assert!(text.contains("push to the same branch"), "{text}");
    }

    /// Both copies carry the remainder — that is the whole "attached to both".
    #[test]
    fn the_pull_request_gets_the_same_unclaimed_set_the_chat_does() {
        let review = reviewed();
        let body = github_body(&review, "Two things.");
        assert!(body.contains("Two things."), "{body}");
        assert!(body.contains("2 changes no claim accounts for"), "{body}");
        assert!(body.contains("Cargo.toml — added, +1 −0"), "{body}");
        assert!(
            body.contains("crates/ui/src/shell.rs — +6 −4, unmentioned"),
            "{body}"
        );
        // And it is recognisable as the board's own, which is what keeps the
        // relay shut when GitHub answers without an id.
        assert!(body.contains(POSTED_MARK), "{body}");
        assert!(crate::review::is_the_boards_own(&body), "{body}");
    }

    /// The sentence the screen states before you press anything.
    #[test]
    fn the_screen_says_where_it_goes_and_how_much_rides_along() {
        let mut review = reviewed();
        assert_eq!(
            contract_line(&review, true),
            "Delivered into board/gh-138 once, with both unclaimed changes attached."
        );
        review.remainder.unclaimed.truncate(1);
        assert_eq!(
            contract_line(&review, true),
            "Delivered into board/gh-138 once, with the unclaimed change attached."
        );
        review.remainder = Remainder::default();
        assert_eq!(
            contract_line(&review, true),
            "Delivered into board/gh-138 once."
        );
        // A verdict nobody is told about must not claim to be delivered.
        assert_eq!(
            contract_line(&review, false),
            "Posted to the pull request. Nothing is delivered into the chat."
        );
    }

    /// §gh#180's rule, applied to a verdict the board is about to write: an
    /// approval with nothing to say does not interrupt anybody.
    #[test]
    fn a_bare_approval_interrupts_nobody_and_a_verdict_with_prose_does() {
        assert!(!worth_delivering(VerdictKind::Approve, "   "));
        assert!(worth_delivering(
            VerdictKind::Approve,
            "Nice — rename `x` first."
        ));
        assert!(worth_delivering(VerdictKind::ChangesRequested, ""));
        assert!(VerdictKind::ChangesRequested.needs_comment());
        assert!(VerdictKind::Comment.needs_comment());
        assert!(!VerdictKind::Approve.needs_comment());
    }

    /// The identity a retry is matched on. Same words, same submission; a
    /// different sentence or a different verdict is a different review.
    #[test]
    fn the_same_verdict_twice_is_one_submission() {
        let a = fingerprint(7, VerdictKind::ChangesRequested, "Fix the gate.");
        assert_eq!(
            a,
            fingerprint(7, VerdictKind::ChangesRequested, "Fix the gate.\n")
        );
        assert_ne!(
            a,
            fingerprint(7, VerdictKind::ChangesRequested, "Fix the gates.")
        );
        assert_ne!(a, fingerprint(7, VerdictKind::Comment, "Fix the gate."));
        assert_ne!(
            a,
            fingerprint(8, VerdictKind::ChangesRequested, "Fix the gate.")
        );
        // Persisted, so it must not move between runs of the same binary or
        // between binaries: this is the value, pinned.
        assert_eq!(
            fingerprint(7, VerdictKind::ChangesRequested, "Fix the gate."),
            "7:changes_requested:10e6b818e2289410"
        );
    }

    #[test]
    fn a_pull_request_is_found_by_its_url_and_falls_back_to_the_task_id() {
        let mut task = crate::review::tests::task_row();
        task.id = "gh:o/r#13".into();
        task.pr_url = Some("https://github.com/other/repo/pull/212".into());
        task.pr_number = Some(212);
        // The URL is the authority: a Linear row's work can land in a repo the
        // task id says nothing about.
        assert_eq!(pr_target(&task), Some(("other/repo".into(), 212)));
        task.pr_url = None;
        assert_eq!(pr_target(&task), Some(("o/r".into(), 212)));
        task.pr_number = None;
        assert_eq!(pr_target(&task), None);
    }

    // ---- the submission, driven through the engine -----------------------

    use crate::review::tests::{FakeRuntime, engine, pull, seed_reviewed_task, state_of};
    use crate::sync::SyncEngine;

    /// The fixture GitHub answers a review post with. The id is the whole
    /// point: it is what gets watermarked.
    const POSTED_ID: i64 = 900;

    fn fixture() -> Vec<(String, serde_json::Value)> {
        vec![(
            "POST /repos/o/r/pulls/14/reviews".into(),
            serde_json::json!({ "id": POSTED_ID }),
        )]
    }

    /// A seeded task whose attempt claimed one of its two changed files, so
    /// there is a remainder to attach.
    fn seed_with_a_remainder(e: &SyncEngine) -> i64 {
        seed_reviewed_task(e, "board/gh-13", "chat-1");
        let attempt = e.db.get_task("gh:o/r#13").unwrap().unwrap().attempts[0].id;
        e.db.set_attempt_changes(
            attempt,
            &[
                changed("crates/board/src/review.rs", "M", 40, 12),
                changed("Cargo.toml", "A", 1, 0),
                changed("crates/ui/src/shell.rs", "M", 6, 4),
            ],
        )
        .unwrap();
        e.db.set_attempt_claims(
            attempt,
            &[Claim {
                text: "The watermark is per endpoint.".into(),
                files: vec!["crates/board/src/review.rs".into()],
                symbols: vec![],
            }],
        )
        .unwrap();
        attempt
    }

    /// The whole verb: one review on GitHub, one prompt in the chat, the
    /// remainder on both.
    #[test]
    fn a_verdict_is_posted_to_github_and_delivered_into_the_authoring_chat() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(fixture()));
        let e = engine(rest.clone());
        seed_with_a_remainder(&e);
        let runtime = FakeRuntime::holding("/wt/gh-13-1");

        let receipt = e
            .submit_verdict(
                Some(&runtime),
                "gh:o/r#13",
                None,
                VerdictKind::ChangesRequested,
                "Why does this need itertools?",
            )
            .unwrap();

        // One review, with the verdict and the remainder in it.
        let wrote = rest.wrote.borrow();
        assert_eq!(wrote.len(), 1, "{wrote:?}");
        assert_eq!(wrote[0].1, "/repos/o/r/pulls/14/reviews");
        assert_eq!(wrote[0].2["event"], "REQUEST_CHANGES");
        let body = wrote[0].2["body"].as_str().unwrap();
        assert!(body.contains("Why does this need itertools?"), "{body}");
        assert!(body.contains("2 changes no claim accounts for"), "{body}");
        assert!(body.contains("Cargo.toml — added, +1 −0"), "{body}");
        drop(wrote);

        // One prompt, carrying the same remainder.
        let prompts = runtime.prompts.borrow();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].0, "chat-1");
        assert!(prompts[0].1.contains("[review · changes requested]"));
        assert!(
            prompts[0]
                .1
                .contains("[unclaimed] Cargo.toml — added, +1 −0")
        );
        assert!(
            prompts[0]
                .1
                .contains("[unclaimed] crates/ui/src/shell.rs — +6 −4, unmentioned")
        );
        assert!(
            !prompts[0]
                .1
                .contains("[unclaimed] crates/board/src/review.rs"),
            "the claimed file is not in the remainder"
        );
        drop(prompts);

        assert!(receipt.recorded && receipt.delivered);
        assert_eq!(receipt.review_id, POSTED_ID);
        assert_eq!(receipt.unclaimed, 2);
        assert_eq!(receipt.chat_id.as_deref(), Some("chat-1"));
    }

    /// Once. A second submission of the same verdict — a double click, a
    /// retried call — posts nothing and says nothing again.
    #[test]
    fn the_same_verdict_submitted_twice_is_posted_once_and_delivered_once() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(fixture()));
        let e = engine(rest.clone());
        seed_with_a_remainder(&e);
        let runtime = FakeRuntime::holding("/wt/gh-13-1");

        let first = e
            .submit_verdict(
                Some(&runtime),
                "gh:o/r#13",
                None,
                VerdictKind::ChangesRequested,
                "Fix the gate.",
            )
            .unwrap();
        let again = e
            .submit_verdict(
                Some(&runtime),
                "gh:o/r#13",
                None,
                VerdictKind::ChangesRequested,
                "Fix the gate.",
            )
            .unwrap();

        assert_eq!(rest.wrote.borrow().len(), 1, "one review, not two");
        assert_eq!(runtime.prompts.borrow().len(), 1, "one prompt, not two");
        assert!(first.recorded);
        assert!(!again.recorded, "the retry found it already submitted");
        assert!(again.delivered);
        assert_eq!(again.review_id, POSTED_ID);

        // A different sentence is a different review, and does go out.
        e.submit_verdict(
            Some(&runtime),
            "gh:o/r#13",
            None,
            VerdictKind::ChangesRequested,
            "Still wrong on the retry path.",
        )
        .unwrap();
        assert_eq!(rest.wrote.borrow().len(), 2);
        assert_eq!(runtime.prompts.borrow().len(), 2);
    }

    /// A delivery that never reached the ledger is the half a retry finishes:
    /// the review is already on GitHub, so the retry must not post a second
    /// one, and the agent still has not been told.
    #[test]
    fn a_verdict_whose_delivery_failed_is_retried_without_a_second_review() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(fixture()));
        let e = engine(rest.clone());
        seed_with_a_remainder(&e);
        let mut runtime = FakeRuntime::holding("/wt/gh-13-1");
        runtime.prompt_fails = true;

        let failed = e
            .submit_verdict(
                Some(&runtime),
                "gh:o/r#13",
                None,
                VerdictKind::ChangesRequested,
                "Fix the gate.",
            )
            .unwrap();
        assert!(failed.recorded && !failed.delivered);
        assert!(failed.not_delivered.is_some());
        assert_eq!(state_of(&e).submissions.len(), 1);
        assert!(!state_of(&e).submissions[0].delivered);

        runtime.prompt_fails = false;
        let retried = e
            .submit_verdict(
                Some(&runtime),
                "gh:o/r#13",
                None,
                VerdictKind::ChangesRequested,
                "Fix the gate.",
            )
            .unwrap();
        assert!(!retried.recorded, "the verdict was already on the ledger");
        assert!(retried.delivered);
        assert_eq!(rest.wrote.borrow().len(), 1);
        assert_eq!(runtime.prompts.borrow().len(), 1);
    }

    /// The author check, on the outbound side: a chat re-pointed at another
    /// checkout is somebody else's session. The review still belongs on the
    /// pull request; the prompt belongs nowhere.
    #[test]
    fn a_chat_repointed_at_another_checkout_is_posted_to_but_not_delivered_into() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(fixture()));
        let e = engine(rest.clone());
        seed_with_a_remainder(&e);
        let runtime = FakeRuntime::holding("/wt/lin-140-1");

        let receipt = e
            .submit_verdict(
                Some(&runtime),
                "gh:o/r#13",
                None,
                VerdictKind::ChangesRequested,
                "Fix the gate.",
            )
            .unwrap();

        assert_eq!(rest.wrote.borrow().len(), 1, "the review still lands");
        assert!(runtime.prompts.borrow().is_empty(), "and nobody is told");
        assert!(!receipt.delivered);
        assert!(
            receipt
                .not_delivered
                .as_deref()
                .is_some_and(|why| why.contains("no longer holds the agent")),
            "{receipt:?}"
        );

        // A chat that is gone entirely reads the same way.
        let gone = FakeRuntime::gone();
        let receipt = e
            .submit_verdict(
                Some(&gone),
                "gh:o/r#13",
                None,
                VerdictKind::ChangesRequested,
                "Another look.",
            )
            .unwrap();
        assert!(!receipt.delivered);
    }

    /// The reason the id is watermarked at post time: the sync loop reads the
    /// reviews endpoint, finds the verdict the board itself just wrote, and
    /// must not hand it to the agent a second time.
    #[test]
    fn the_boards_own_verdict_never_comes_back_through_the_inbound_path() {
        let mut routes = fixture();
        routes.extend([
            (
                "/repos/o/r/issues/14/comments".to_string(),
                serde_json::json!([]),
            ),
            (
                "/repos/o/r/pulls/14/comments".to_string(),
                serde_json::json!([]),
            ),
            // GitHub reporting back the review the board just posted.
            (
                "/repos/o/r/pulls/14/reviews".to_string(),
                serde_json::json!([{ "id": POSTED_ID, "state": "CHANGES_REQUESTED",
                                     "body": "Fix the gate.\n\n— written in comet-board's \
                                              review window.",
                                     "submitted_at": "2999-01-01T00:00:00Z",
                                     "user": { "login": "b" } }]),
            ),
        ]);
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(routes));
        let e = engine(rest.clone());
        seed_with_a_remainder(&e);
        let runtime = FakeRuntime::holding("/wt/gh-13-1");

        e.submit_verdict(
            Some(&runtime),
            "gh:o/r#13",
            None,
            VerdictKind::ChangesRequested,
            "Fix the gate.",
        )
        .unwrap();
        assert_eq!(runtime.prompts.borrow().len(), 1);
        assert_eq!(state_of(&e).review, POSTED_ID, "watermarked on the way out");

        // The very next sync cycle sees the pull request move and reads all
        // three endpoints. The verdict is at the watermark, so it is consumed
        // rather than relayed.
        e.deliver_reviews(&runtime, &[pull("board/gh-13")]);
        assert_eq!(
            runtime.prompts.borrow().len(),
            1,
            "delivered once, from the review window — not again from the poll"
        );
    }

    /// The other side of that watermark, and why §gh#289 could not simply read
    /// the reviews endpoint: a verdict written here is consumed the moment it is
    /// posted, so the inbound pass never sees it and would never learn to tell
    /// the layers stacked on this one. The *standing* verdict is what closes
    /// that — recorded here, fanned out there, one path for both sources.
    #[test]
    fn a_verdict_written_here_reaches_the_layers_stacked_on_it() {
        let mut routes = fixture();
        routes.extend([
            (
                "/repos/o/r/issues/14/comments".to_string(),
                serde_json::json!([]),
            ),
            (
                "/repos/o/r/pulls/14/comments".to_string(),
                serde_json::json!([]),
            ),
            (
                "/repos/o/r/pulls/14/reviews".to_string(),
                serde_json::json!([{ "id": POSTED_ID, "state": "CHANGES_REQUESTED",
                                     "body": "Fix the gate.\n\n— written in comet-board's \
                                              review window.",
                                     "submitted_at": "2999-01-01T00:00:00Z",
                                     "user": { "login": "b" } }]),
            ),
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
        seed_with_a_remainder(&e);
        crate::review::tests::seed_stacked_child(&e, "chat-2");
        let runtime =
            FakeRuntime::holding_each(&[("chat-1", "/wt/gh-13-1"), ("chat-2", "/wt/gh-20-1")]);

        e.submit_verdict(
            Some(&runtime),
            "gh:o/r#13",
            None,
            VerdictKind::ChangesRequested,
            "Fix the gate.",
        )
        .unwrap();
        assert!(runtime.said_to("chat-1").is_some(), "the author, from here");
        assert_eq!(
            state_of(&e).changes_requested,
            Some(POSTED_ID),
            "and the standing verdict, so the fan-out has something to run off",
        );

        // The next sync cycle's pass fans it up the stack, without reading the
        // review it can never read.
        e.deliver_reviews(&runtime, &crate::review::tests::stacked_pulls());
        assert!(
            runtime
                .said_to("chat-2")
                .unwrap()
                .contains("the layer below yours was asked to change"),
            "{:?}",
            runtime.said_to("chat-2"),
        );
        assert_eq!(
            e.db.get_task("gh:o/r#20").unwrap().unwrap().state,
            crate::model::BoardState::Blocked,
        );

        // An approval withdraws it, and the layer above is reviewable again.
        e.submit_verdict(
            Some(&runtime),
            "gh:o/r#13",
            None,
            VerdictKind::Approve,
            "Better.",
        )
        .unwrap();
        assert_eq!(state_of(&e).changes_requested, None);
        assert_eq!(
            e.db.get_task("gh:o/r#20").unwrap().unwrap().state,
            crate::model::BoardState::Review,
        );
    }

    /// The two refusals, both of which would otherwise be a lie told to an
    /// agent or a 422 from GitHub.
    #[test]
    fn a_closed_pull_request_and_an_empty_verdict_are_refused() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(fixture()));
        let e = engine(rest.clone());
        seed_with_a_remainder(&e);
        let runtime = FakeRuntime::holding("/wt/gh-13-1");

        let err = e
            .submit_verdict(
                Some(&runtime),
                "gh:o/r#13",
                None,
                VerdictKind::ChangesRequested,
                "   ",
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("needs something written in it"), "{err}");

        e.db.set_pr(
            "gh:o/r#13",
            Some("https://github.com/o/r/pull/14"),
            Some(14),
            false,
        )
        .unwrap();
        e.rederive_all().unwrap();
        let err = e
            .submit_verdict(
                Some(&runtime),
                "gh:o/r#13",
                None,
                VerdictKind::ChangesRequested,
                "Fix the gate.",
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("is closed"), "{err}");
        assert!(rest.wrote.borrow().is_empty(), "nothing was posted");
    }

    /// An approval with nothing to say is posted and interrupts nobody — the
    /// inbound path's rule, kept on the way out.
    #[test]
    fn a_bare_approval_is_posted_and_delivered_to_nobody() {
        let rest = std::rc::Rc::new(crate::sources::github::FixtureRest::new(fixture()));
        let e = engine(rest.clone());
        seed_with_a_remainder(&e);
        let runtime = FakeRuntime::holding("/wt/gh-13-1");

        let receipt = e
            .submit_verdict(Some(&runtime), "gh:o/r#13", None, VerdictKind::Approve, "")
            .unwrap();

        assert_eq!(rest.wrote.borrow()[0].2["event"], "APPROVE");
        assert!(runtime.prompts.borrow().is_empty());
        assert!(!receipt.delivered);
        assert_eq!(
            receipt.not_delivered.as_deref(),
            Some("an approval with nothing to say interrupts nobody")
        );
    }

    // ---- gh#365: the projection is not the verdict -----------------------

    /// GitHub's own words for the refusal every board-dispatched pull request
    /// gets: the App submitting is the App that opened it.
    const OWN_PR: &str = "Can not approve your own pull request";

    /// The whole inversion, on the case that made it necessary. GitHub refuses
    /// the write, and every other consequence of the verdict happens anyway:
    /// it is on the ledger, the objection is standing, and the sentence the
    /// human typed is in the checkout the agent is still in.
    #[test]
    fn a_verdict_github_refuses_still_stands_and_still_reaches_the_agent() {
        let rest = std::rc::Rc::new(
            crate::sources::github::FixtureRest::new(fixture())
                .refusing("REQUEST_CHANGES", "Something GitHub would not take"),
        );
        let e = engine(rest.clone());
        seed_with_a_remainder(&e);
        let runtime = FakeRuntime::holding("/wt/gh-13-1");

        let receipt = e
            .submit_verdict(
                Some(&runtime),
                "gh:o/r#13",
                None,
                VerdictKind::ChangesRequested,
                "Why does this need itertools?",
            )
            .unwrap();

        // The verdict, and everything downstream of it that is not GitHub's.
        assert!(receipt.recorded, "{receipt:?}");
        assert!(receipt.delivered, "the agent was told");
        let prompts = runtime.prompts.borrow();
        assert_eq!(prompts.len(), 1);
        assert!(
            prompts[0].1.contains("Why does this need itertools?"),
            "the reviewer's own words reached the checkout: {}",
            prompts[0].1
        );
        drop(prompts);
        let state = state_of(&e);
        assert_eq!(state.submissions.len(), 1, "recorded, not lost");
        assert!(state.submissions[0].delivered);
        assert!(
            state.changes_requested.is_some(),
            "the objection stands whatever GitHub said",
        );

        // And the projection, said out loud rather than swallowed.
        assert_eq!(receipt.projection, Projection::Unposted);
        assert_eq!(state.submissions[0].projection, Projection::Unposted);
        assert!(
            receipt
                .refused
                .as_deref()
                .is_some_and(|why| why.contains("Something GitHub would not take")),
            "{receipt:?}"
        );
        assert_eq!(receipt.review_id, 0, "there is no review behind it");
        assert_eq!(state.review, 0, "and nothing to watermark");
        // The one surface rule that follows: the words stay where the reviewer
        // can retry them or paste them onto GitHub by hand.
        assert!(keeps_the_comment(&receipt));
    }

    /// The other half of that: a projection is the one part of a submission a
    /// retry still has work to do on, and finishing it posts one review.
    #[test]
    fn a_refused_projection_is_finished_by_the_retry_that_records_nothing() {
        let rest = std::rc::Rc::new(
            crate::sources::github::FixtureRest::new(fixture())
                .refusing("REQUEST_CHANGES", "Something GitHub would not take"),
        );
        let e = engine(rest.clone());
        seed_with_a_remainder(&e);
        let runtime = FakeRuntime::holding("/wt/gh-13-1");

        let refused = e
            .submit_verdict(
                Some(&runtime),
                "gh:o/r#13",
                None,
                VerdictKind::ChangesRequested,
                "Fix the gate.",
            )
            .unwrap();
        assert_eq!(refused.projection, Projection::Unposted);

        rest.refused.borrow_mut().clear();
        let retried = e
            .submit_verdict(
                Some(&runtime),
                "gh:o/r#13",
                None,
                VerdictKind::ChangesRequested,
                "Fix the gate.",
            )
            .unwrap();

        assert!(!retried.recorded, "the verdict was already on the ledger");
        assert_eq!(retried.projection, Projection::Posted);
        assert_eq!(retried.review_id, POSTED_ID);
        assert_eq!(runtime.prompts.borrow().len(), 1, "told once, not twice");
        assert_eq!(state_of(&e).submissions.len(), 1, "one submission");
        let state = state_of(&e);
        assert_eq!(state.review, POSTED_ID, "watermarked once it exists");
        assert_eq!(
            state.changes_requested,
            Some(POSTED_ID),
            "and the standing verdict moves onto GitHub's number for it, so the \
             inbound pass reading its own review back agrees with the board",
        );
        // Nothing more is posted on a third try.
        e.submit_verdict(
            Some(&runtime),
            "gh:o/r#13",
            None,
            VerdictKind::ChangesRequested,
            "Fix the gate.",
        )
        .unwrap();
        assert_eq!(
            rest.wrote.borrow().len(),
            2,
            "the refused attempt and the one that landed — no third",
        );
    }

    /// The refusal the board can name: an App may not approve a pull request
    /// its own App opened. Answered rather than reported — the verdict goes out
    /// as a comment that says, in words, that it is an approval.
    #[test]
    fn an_approval_the_app_may_not_cast_travels_as_a_comment_that_says_it_approves() {
        let rest = std::rc::Rc::new(
            crate::sources::github::FixtureRest::new(fixture()).refusing("APPROVE", OWN_PR),
        );
        let e = engine(rest.clone());
        seed_with_a_remainder(&e);
        let runtime = FakeRuntime::holding("/wt/gh-13-1");

        let receipt = e
            .submit_verdict(
                Some(&runtime),
                "gh:o/r#13",
                None,
                VerdictKind::Approve,
                "Reads right.",
            )
            .unwrap();

        let wrote = rest.wrote.borrow();
        assert_eq!(wrote.len(), 2, "the approval, then the comment: {wrote:?}");
        assert_eq!(wrote[0].2["event"], "APPROVE");
        assert_eq!(wrote[1].2["event"], "COMMENT");
        let body = wrote[1].2["body"].as_str().unwrap();
        assert!(body.contains("This is an approval."), "{body}");
        assert!(
            body.contains("does not let this App approve a pull request its own App opened"),
            "{body}"
        );
        assert!(body.contains("recorded as approved"), "{body}");
        // Everything the verdict already carried is still in it, mark included,
        // so the inbound path's backstop is untouched by the downgrade.
        assert!(body.contains("Reads right."), "{body}");
        assert!(body.contains("2 changes no claim accounts for"), "{body}");
        assert!(crate::review::is_the_boards_own(body), "{body}");
        drop(wrote);

        assert_eq!(receipt.projection, Projection::PostedAsComment);
        assert_eq!(receipt.review_id, POSTED_ID);
        assert!(
            receipt
                .refused
                .as_deref()
                .is_some_and(|w| w.contains(OWN_PR)),
            "GitHub's own reason is kept: {receipt:?}",
        );
        let state = state_of(&e);
        assert_eq!(state.review, POSTED_ID, "still watermarked");
        assert_eq!(state.changes_requested, None, "and it still withdraws");
        assert!(!keeps_the_comment(&receipt), "GitHub has a copy");
        // And the reviewer is told which of the two happened, without having to
        // know GitHub's rules about who may review what.
        let line = receipt_line(&receipt);
        assert!(
            line.contains("as a comment that says it approves"),
            "{line}"
        );
        assert!(
            line.contains("does not let the board approve its own pull request"),
            "{line}"
        );
    }

    /// The same refusal on the other dead button. `REQUEST_CHANGES` is refused
    /// for the same reason and answered the same way — leaving it out would fix
    /// one of the two buttons gh#365 found dead.
    #[test]
    fn a_request_for_changes_the_app_may_not_cast_travels_as_a_comment_too() {
        let rest = std::rc::Rc::new(
            crate::sources::github::FixtureRest::new(fixture()).refusing(
                "REQUEST_CHANGES",
                "Can not request changes on your own pull request",
            ),
        );
        let e = engine(rest.clone());
        seed_with_a_remainder(&e);
        let runtime = FakeRuntime::holding("/wt/gh-13-1");

        let receipt = e
            .submit_verdict(
                Some(&runtime),
                "gh:o/r#13",
                None,
                VerdictKind::ChangesRequested,
                "Fix the gate.",
            )
            .unwrap();

        let wrote = rest.wrote.borrow();
        assert_eq!(wrote[1].2["event"], "COMMENT");
        let body = wrote[1].2["body"].as_str().unwrap();
        assert!(body.contains("This asks for changes."), "{body}");
        assert!(body.contains("recorded as changes requested"), "{body}");
        drop(wrote);

        assert_eq!(receipt.projection, Projection::PostedAsComment);
        assert_eq!(
            state_of(&e).changes_requested,
            Some(POSTED_ID),
            "the objection stands under the id of the comment carrying it",
        );
        // A `commented` review is not a verdict the inbound pass reads back, so
        // nothing there can withdraw this one behind the board's back.
        assert!(receipt.delivered);
    }

    /// A `COMMENT` GitHub refuses has nothing to fall back to, and says so
    /// rather than posting a second one.
    #[test]
    fn a_refused_comment_is_not_retried_as_a_comment() {
        let rest = std::rc::Rc::new(
            crate::sources::github::FixtureRest::new(fixture())
                .refusing("COMMENT", "Something GitHub would not take"),
        );
        let e = engine(rest.clone());
        seed_with_a_remainder(&e);
        let runtime = FakeRuntime::holding("/wt/gh-13-1");

        let receipt = e
            .submit_verdict(
                Some(&runtime),
                "gh:o/r#13",
                None,
                VerdictKind::Comment,
                "One thought.",
            )
            .unwrap();

        assert_eq!(rest.wrote.borrow().len(), 1, "asked once");
        assert_eq!(receipt.projection, Projection::Unposted);
        assert!(receipt.delivered, "and the agent still has it");
    }

    /// A verdict GitHub has not numbered still fans out — and when GitHub
    /// finally numbers it, the fan-out does not run again under the new name.
    ///
    /// The standing verdict is a watermark as well as a fact, so an unposted
    /// one stands under an id the board made up. Moving it onto GitHub's number
    /// without moving what was fanned out under it would tell every layer above
    /// this one the same thing twice.
    #[test]
    fn a_verdict_that_stands_under_a_local_id_fans_out_once_across_the_move() {
        let mut routes = fixture();
        for number in [14, 15] {
            routes.extend([
                (
                    format!("/repos/o/r/issues/{number}/comments"),
                    serde_json::json!([]),
                ),
                (
                    format!("/repos/o/r/pulls/{number}/comments"),
                    serde_json::json!([]),
                ),
                (
                    format!("/repos/o/r/pulls/{number}/reviews"),
                    serde_json::json!([]),
                ),
            ]);
        }
        let rest = std::rc::Rc::new(
            crate::sources::github::FixtureRest::new(routes)
                .refusing("REQUEST_CHANGES", "Something GitHub would not take"),
        );
        let e = engine(rest.clone());
        seed_with_a_remainder(&e);
        crate::review::tests::seed_stacked_child(&e, "chat-2");
        let runtime =
            FakeRuntime::holding_each(&[("chat-1", "/wt/gh-13-1"), ("chat-2", "/wt/gh-20-1")]);

        let refused = e
            .submit_verdict(
                Some(&runtime),
                "gh:o/r#13",
                None,
                VerdictKind::ChangesRequested,
                "Fix the gate.",
            )
            .unwrap();
        assert_eq!(refused.projection, Projection::Unposted);
        let local = state_of(&e).changes_requested.expect("it still stands");

        // The layer above hears about it on the next cycle, off a verdict
        // GitHub has never seen.
        e.deliver_reviews(&runtime, &crate::review::tests::stacked_pulls());
        assert!(
            runtime
                .said_to("chat-2")
                .unwrap()
                .contains("the layer below yours was asked to change"),
        );
        assert_eq!(state_of(&e).fanned_out.get("gh:o/r#20"), Some(&local));

        // The retry lands the projection, and everything keyed by the local id
        // moves with it — including what has already been fanned out.
        rest.refused.borrow_mut().clear();
        e.submit_verdict(
            Some(&runtime),
            "gh:o/r#13",
            None,
            VerdictKind::ChangesRequested,
            "Fix the gate.",
        )
        .unwrap();
        let state = state_of(&e);
        assert_eq!(state.changes_requested, Some(POSTED_ID));
        assert_eq!(state.fanned_out.get("gh:o/r#20"), Some(&POSTED_ID));

        let told = runtime.prompts.borrow().len();
        e.deliver_reviews(&runtime, &crate::review::tests::stacked_pulls());
        assert_eq!(
            runtime.prompts.borrow().len(),
            told,
            "the stack is not told a second time about one verdict",
        );
    }

    /// The sentence every surface prints, in the order gh#365 puts it: what the
    /// board did, then what GitHub did with the copy.
    #[test]
    fn the_receipt_says_the_verdict_first_and_the_projection_second() {
        let receipt = |projection, refused: Option<&str>| VerdictReceipt {
            task_id: "gh:o/r#13".into(),
            attempt: 7,
            kind: VerdictKind::Approve,
            review_id: 900,
            recorded: true,
            chat_id: Some("chat-1".into()),
            delivered: true,
            not_delivered: None,
            projection,
            refused: refused.map(str::to_string),
            unclaimed: 0,
            payload: String::new(),
        };
        assert_eq!(
            receipt_line(&receipt(Projection::Posted, None)),
            "Recorded, and delivered into the chat once. It is on the pull request."
        );
        assert_eq!(
            receipt_line(&receipt(Projection::Unposted, Some("github HTTP 500"))),
            "Recorded, and delivered into the chat once. It is not on the pull request \
             — GitHub refused it: github HTTP 500"
        );
        assert_eq!(
            receipt_line(&receipt(Projection::PostedAsComment, Some(OWN_PR))),
            "Recorded, and delivered into the chat once. It is on the pull request as a \
             comment that says it approves — GitHub does not let the board approve its \
             own pull request."
        );
    }

    /// The one refusal the board answers instead of reporting, read off
    /// GitHub's own sentence.
    #[test]
    fn only_a_refusal_about_reviewing_your_own_pull_request_is_answered() {
        let said = |why: &str| {
            anyhow::anyhow!("github HTTP 422 for /repos/o/r/pulls/14/reviews: {why}")
                .context("submitting the APPROVE verdict on o/r#14")
        };
        assert!(refused_own_pull_request(&said(OWN_PR)));
        assert!(refused_own_pull_request(&said(
            "Validation Failed — user_id: Can not request changes on your own pull request"
        )));
        assert!(!refused_own_pull_request(&said(
            "Validation Failed — body: can't be blank"
        )));
        assert!(!refused_own_pull_request(&anyhow::anyhow!(
            "github HTTP 500 for /repos/o/r/pulls/14/reviews"
        )));
    }

    #[test]
    fn a_verdict_reads_off_the_wire_in_either_spelling() {
        assert_eq!(
            VerdictKind::parse("changes_requested"),
            Some(VerdictKind::ChangesRequested)
        );
        assert_eq!(
            VerdictKind::parse("request_changes"),
            Some(VerdictKind::ChangesRequested)
        );
        assert_eq!(VerdictKind::parse("approve"), Some(VerdictKind::Approve));
        assert_eq!(VerdictKind::parse("comment"), Some(VerdictKind::Comment));
        assert_eq!(VerdictKind::parse("lgtm"), None);
        assert_eq!(VerdictKind::ChangesRequested.event(), "REQUEST_CHANGES");
    }
}
