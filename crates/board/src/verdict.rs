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
//! ## What it refuses
//!
//! A closed pull request (the payload's promise that the branch is still live
//! would be a lie), a `comment` or `changes requested` with no prose (GitHub
//! refuses those itself, and a verdict that is only a verdict is not one), and
//! a chat that has moved: `chat_alive` plus [`crate::review::
//! still_the_authors_checkout`], the same author check the inbound path makes,
//! because pasting a review into a session that has moved on to other work
//! delivers it to the wrong author. The review is still posted — the pull
//! request is where it belongs regardless — and the receipt says nobody was
//! told.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::claims::{AttemptReview, ChangedFile};
use crate::model::Task;
use crate::review::still_the_authors_checkout;
use crate::runtime::Runtime;
use crate::sync::SyncEngine;

/// The trailer every verdict posted through the board carries.
///
/// True of every one of them and of nothing else on a pull request, which is
/// what makes it usable as the backstop for a GitHub reply that carried no id.
/// It says only what the board can promise at post time — the delivery may
/// still fail, and lying about that on the pull request would be worse than
/// saying nothing.
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
    /// which case [`POSTED_MARK`] is what keeps the relay shut.
    pub review_id: i64,
    /// Whether the authoring chat has been told. False after a post whose
    /// delivery failed — the half a retry finishes.
    pub delivered: bool,
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
    /// The GitHub review id, or `0` when GitHub did not say.
    pub review_id: i64,
    /// Was this the call that posted it? `false` when a retry found the
    /// submission already recorded, which is what idempotency looks like from
    /// the outside.
    pub posted: bool,
    /// The chat the payload went into.
    pub chat_id: Option<String>,
    pub delivered: bool,
    /// Why nobody was told. `None` when somebody was.
    pub not_delivered: Option<String>,
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
    /// Submit a verdict: post it on the pull request, and hand it to the agent
    /// that is still in the checkout (§gh#239).
    ///
    /// `attempt` names one; `None` is the task's latest, which is what a
    /// reviewer looking at a row means. The remainder is recomputed here rather
    /// than taken from the caller — a reviewer cannot be asked to retype the
    /// unclaimed set, and a caller that could supply it could also get it
    /// wrong.
    ///
    /// Order matters and is the recoverable one: post, record, then deliver.
    /// A crash between the post and the record costs one duplicate review, a
    /// window of milliseconds; recording first would mean a failed post is
    /// remembered as a success and the verdict never reaches GitHub at all.
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
        let task = self
            .db
            .get_task(task_id)?
            .ok_or_else(|| anyhow::anyhow!("{task_id} is not on the board"))?;
        let Some(gh) = &self.github else {
            bail!("this board has no GitHub credential, so it cannot post a review");
        };
        let (repo, number) = pr_target(&task).ok_or_else(|| {
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

        // ---- post -----------------------------------------------------
        let (review_id, posted) = match &already {
            Some(prior) => (prior.review_id, false),
            None => {
                let id =
                    gh.post_review(&repo, number, kind.event(), &github_body(&review, comment))?;
                (id, true)
            }
        };
        if posted {
            // The watermark, before anything else can fail: an id at or below
            // it can never come back through `deliver_reviews`, which is what
            // makes this verdict arrive once rather than once from here and
            // again from the poll that finds it.
            state.review = state.review.max(review_id);
            state.submissions.push(Submission {
                fingerprint: print.clone(),
                review_id,
                delivered: false,
            });
            if state.submissions.len() > MAX_SUBMISSIONS {
                let drop = state.submissions.len() - MAX_SUBMISSIONS;
                state.submissions.drain(..drop);
            }
            crate::review::store(&self.db, &task.id, &state)?;
            self.log.info(format!(
                "{}: posted a `{}` review on {repo}#{number}",
                task.identifier,
                kind.label()
            ));
        }

        // ---- deliver --------------------------------------------------
        let mut receipt = VerdictReceipt {
            task_id: task.id.clone(),
            attempt: review.attempt,
            kind,
            review_id,
            posted,
            chat_id: attempt_row.and_then(|a| a.pane_id.clone()),
            delivered: already.as_ref().is_some_and(|s| s.delivered),
            not_delivered: None,
            unclaimed: review.remainder.unclaimed.len(),
            payload: payload.clone(),
        };
        if receipt.delivered {
            return Ok(receipt);
        }
        if !worth_delivering(kind, comment) {
            receipt.not_delivered =
                Some("an approval with nothing to say interrupts nobody".into());
            return Ok(receipt);
        }
        let (Some(runtime), Some(attempt_row)) = (runtime, attempt_row) else {
            receipt.not_delivered = Some("there is no live runtime to deliver into".into());
            return Ok(receipt);
        };
        let Some(chat_id) = attempt_row.pane_id.as_deref() else {
            receipt.not_delivered = Some("this attempt never had a chat".into());
            return Ok(receipt);
        };
        // The same author check the inbound path makes, and for the same
        // reason: a chat that has been re-pointed at another checkout is
        // somebody else's session, and a review pasted into it is delivered to
        // the wrong author.
        //
        // A runtime that cannot answer is reported on the receipt rather than
        // raised: the review is already on GitHub, and an error here would
        // tell the reviewer nothing happened at all.
        let here = runtime.chat_alive(chat_id).and_then(|alive| {
            Ok(alive
                && still_the_authors_checkout(runtime.chat_cwd(chat_id)?.as_deref(), attempt_row))
        });
        let here = match here {
            Ok(here) => here,
            Err(e) => {
                receipt.not_delivered = Some(format!("the chat could not be asked about: {e}"));
                return Ok(receipt);
            }
        };
        if !here {
            receipt.not_delivered = Some(format!(
                "chat {chat_id} no longer holds the agent that wrote this branch"
            ));
            self.log.info(format!(
                "{}: verdict posted on {repo}#{number}, but {}",
                task.identifier,
                receipt.not_delivered.as_deref().unwrap_or_default()
            ));
            return Ok(receipt);
        }
        if let Err(e) = runtime.prompt(chat_id, &payload) {
            // Posted and undelivered is the state a retry exists for: the
            // submission is recorded, so the retry skips the post and tries
            // only the half that failed.
            receipt.not_delivered = Some(format!("the chat could not be queued: {e}"));
            return Ok(receipt);
        }
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
        Ok(receipt)
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

        assert!(receipt.posted && receipt.delivered);
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
        assert!(first.posted);
        assert!(!again.posted, "the retry found it already submitted");
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
        assert!(failed.posted && !failed.delivered);
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
        assert!(!retried.posted, "the review was already on GitHub");
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
