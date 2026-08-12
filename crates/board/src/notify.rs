//! Who gets told what, when a dispatched attempt blocks or settles (gh#71),
//! plus the one chat that hears about all of it (gh#104).
//!
//! Four audiences, and they are genuinely different people:
//!
//! 1. **The task.** A blocked attempt posts one comment on its own issue —
//!    the same trail the dispatch and outcome comments leave, and the only one
//!    that survives the night. It is not conditional on who is watching: the
//!    comment belongs to the task. Composed here ([`upstream_comment`]),
//!    delivered by the writeback queue, keyed so a block that lasts three
//!    hours is still one comment (`Attempt::blocked_count`).
//! 2. **The agent that released the work.** An orchestrator cannot be woken —
//!    it only gets a turn when something prompts it — so a settle or a block it
//!    is waiting on has to arrive as a prompt into its own chat
//!    ([`dispatcher_message`]). This is herdr-board's AGE-25 dispatcher wake;
//!    `[defaults] notify_dispatcher` is still the switch, and since gh#165 it
//!    is on by default and tried **first**, because it is the precise channel:
//!    it prompts the one agent whose plan that task was a step in, and nobody
//!    else. *Every* ending, since gh#194 — a run that finished, a chat that
//!    vanished, a cancel somebody pressed, a duration cap that bit — because
//!    what this reader is waiting on is the step being over, not the step
//!    going well.
//! 3. **The pinned orchestrator.** The agent whose job is the *board*, not one
//!    task on it ([`orchestrator_message`]). It is the **addressee of last
//!    resort**, and that is the whole of its job: work no agent released (the
//!    board panel, the phone, a bare `comet-board dispatch`), work whose
//!    dispatcher did not survive it — attempts cap at 2h and chats archive as
//!    their task settles, so a long child routinely outlives its parent — and
//!    the events that belong to no attempt at all, which is the duration cap's
//!    warning ([`Event`]). One chat, named by `[defaults] orchestrator_chat`.
//!
//!    What audience 2 was told is *not* repeated here. That is what makes a
//!    pinned orchestrator survivable on a busy board: its context fills with
//!    the things that would otherwise vanish, not with a copy of every child's
//!    settle. Before gh#165 both channels fired and the notice audience 2 could
//!    not be given was dropped — the two halves of one wrong shape.
//! 4. **The operator, out of band.** None of the above reaches a human who is
//!    not looking at the board or at GitHub. `[defaults] notify_webhook` is
//!    one URL, POSTed [`webhook_payload`] on both events.
//!
//! ## Why one webhook and no integrations
//!
//! Slack, ntfy, email relays and pagers all already accept a POST — that is
//! what an incoming webhook *is*. A board carrying a client per destination
//! would be a board holding three credentials it never reads, each with its own
//! failure mode, to reach the operator who could have pointed one URL at any of
//! them. So: one URL, a small stable JSON body, and a `text` field for the
//! endpoints that only render that.
//!
//! ## Why the webhook is not retried
//!
//! The writeback queue retries because a comment on an issue is worth the same
//! tomorrow. A notification is not: "the agent has been blocked for four
//! seconds", delivered forty minutes late after a backoff chain, is worse than
//! nothing, because it reads as current. A failed POST is logged and dropped,
//! and the upstream comment — which *is* retried — remains the durable trail.

use crate::model::{Attempt, Outcome, Task};
use crate::settled::Evidence;
use anyhow::{Context, Result};
use serde_json::{Value, json};

/// How long the board will wait on a webhook endpoint before giving up on it.
///
/// The POST happens on the board loop, so this is also the longest a wedged
/// endpoint can hold up a sync cycle. Short on purpose: the loop's job is the
/// board, and a notification channel that cannot answer in five seconds has
/// already failed the operator it exists to reach.
const WEBHOOK_TIMEOUT_SECS: u64 = 5;

/// What happened to a dispatched attempt that somebody should hear about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    /// The attempt stopped and cannot continue on its own. It keeps its
    /// concurrency slot and its chat; nothing settles; no outcome writeback
    /// fires. Without a notice this is the silent state — the one an operator
    /// discovers hours later by looking at the board.
    Blocked(Stopped),
    /// The board closed the attempt.
    Settled {
        outcome: Outcome,
        /// What it closed on, when it closed on evidence. `None` for an
        /// attempt the board ended for another reason (its chat vanished).
        evidence: Option<Evidence>,
        pr_url: Option<String>,
        /// Why it ended, for the closes no evidence describes: the duration
        /// cap, an operator's cancel (gh#194). `None` for a settle, where
        /// `evidence` is the answer and a second phrasing of it would only be
        /// a way for the two to disagree.
        ///
        /// The same clause the outcome comment carries upstream, so the agent
        /// reading its chat and the human reading the issue are told the same
        /// thing about the same close.
        note: Option<String>,
    },
}

/// The two ways an attempt blocks. Both read [`crate::model::AgentStatus`]
/// `Blocked`; only the run journal tells them apart, so a board with no
/// runtime to ask reports [`Stopped::Unknown`] rather than guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    /// The run is alive and waiting on an answer — a question, or an approval
    /// the harness will not grant itself.
    Asking,
    /// The run died. The chat is intact with its full context, so this is a
    /// retry-or-cancel decision rather than a failed attempt.
    Errored,
    /// Blocked, and the journal could not be read to say which.
    Unknown,
}

impl Stopped {
    pub fn as_str(self) -> &'static str {
        match self {
            Stopped::Asking => "asking",
            Stopped::Errored => "errored",
            Stopped::Unknown => "unknown",
        }
    }

    /// Read back out of a queued writeback's payload. An unrecognised value is
    /// [`Stopped::Unknown`] rather than an error: the comment is still worth
    /// posting, and its wording covers both cases.
    pub fn parse(s: &str) -> Stopped {
        match s {
            "asking" => Stopped::Asking,
            "errored" => Stopped::Errored,
            _ => Stopped::Unknown,
        }
    }

    /// The clause both the upstream comment and the webhook's `text` use, so
    /// the two channels never describe the same block differently.
    fn phrase(self) -> &'static str {
        match self {
            Stopped::Asking => {
                "the agent is waiting on an answer in its chat and will not go on until it \
                 gets one"
            }
            Stopped::Errored => {
                "the run stopped with an error — the chat still holds the whole task, so it \
                 is a retry or a cancel, not a lost attempt"
            }
            Stopped::Unknown => {
                "the agent has stopped — either waiting on an answer or ended by an error; \
                 the chat will say which"
            }
        }
    }
}

impl Signal {
    /// The webhook event name, and the wording the issue tracker sees.
    pub fn event(&self) -> &'static str {
        match self {
            Signal::Blocked(_) => "on_blocked",
            Signal::Settled { .. } => "on_settled",
        }
    }

    /// What this settle asserts, as one line — the mark that tells a settle
    /// worth sending from one that has already been sent (§gh#356).
    ///
    /// An attempt can settle more than once: a settled chat that works again is
    /// re-opened rather than re-dispatched (§settle-logic's inverse), and every
    /// re-close runs the settle path afresh. Some of those repeats are the
    /// feature working — a review was delivered, the agent pushed a fix, the
    /// attempt settled again — and some are the same close said twice, because
    /// the pull request that settled it the first time is still open and still
    /// settles it instantly. The two are indistinguishable from the signal
    /// alone, which is why this exists: **everything the notice asserts, plus
    /// the branch head it asserts it about.** Equal prints mean a dispatcher
    /// woken twice would be read the same sentence about the same commit.
    ///
    /// `head` is the attempt checkout's own `HEAD`, not origin's. It moves in a
    /// superset of the cases origin's does — work has to exist locally before it
    /// can be pushed — and the direction of that error is the safe one: a
    /// commit that never left the box costs a repeat notice, where reading a
    /// stale tracking ref would cost a suppressed real one. `None` (no
    /// checkout, or a git that would not answer) narrows the print rather than
    /// widening it, so an unreadable checkout also fails towards sending.
    ///
    /// `None` for a block: those are already told once per block by
    /// [`Attempt::blocked_count`], which counts a *state* rather than an event
    /// and needs no mark of its own.
    pub fn settle_print(&self, head: Option<&str>) -> Option<String> {
        let Signal::Settled {
            outcome,
            evidence,
            pr_url,
            note,
        } = self
        else {
            return None;
        };
        Some(format!(
            "{}|{}|{}|{}|{}",
            outcome.as_str(),
            evidence.map(Evidence::as_str).unwrap_or("-"),
            pr_url.as_deref().unwrap_or("-"),
            head.unwrap_or("-"),
            note.as_deref().unwrap_or("-"),
        ))
    }
}

/// One line, for a webhook that renders nothing but text.
pub fn summary(task: &Task, signal: &Signal) -> String {
    let n = attempt_no(task);
    match signal {
        Signal::Blocked(why) => format!(
            "{} is blocked — {}. ({} · attempt {n})",
            task.identifier,
            why.phrase(),
            task.title,
        ),
        Signal::Settled {
            outcome,
            pr_url,
            note,
            ..
        } => {
            let tail = match (outcome, pr_url.as_deref()) {
                (Outcome::Done, Some(url)) => format!(" · {url}"),
                (Outcome::Done, None) => " · no pull request".into(),
                _ => String::new(),
            };
            format!(
                "{} settled as {}{} ({} · attempt {n}){tail}",
                task.identifier,
                outcome.as_str(),
                why(note.as_deref()),
                task.title,
            )
        }
    }
}

/// The comment a blocked attempt leaves on its own issue.
///
/// Written for whoever opens the issue tomorrow, not for a machine: what
/// stopped, where the conversation is, and that the board is not going to
/// resolve it on its own. Prefixed `comet-board:` like every other comment the
/// board writes, which is what keeps review delivery from relaying it back
/// into a chat as though a human had said it (see [`crate::review`]).
/// `attempt_no` is passed rather than counted from the task: the comment is
/// composed when the writeback is *delivered*, and a retry between queueing
/// and delivery would otherwise renumber the attempt the comment is about.
pub fn upstream_comment(attempt_no: u64, why: Stopped, log: &str) -> String {
    format!(
        "comet-board: attempt {attempt_no} is blocked — {}. Nothing further will happen on \
         this until someone picks it up: answer in the chat, or `comet-board cancel` it. \
         · log: {log}",
        why.phrase(),
    )
}

/// The comment an attempt leaves when the board cannot account for the
/// credential that pushed its work (gh#233).
///
/// Addressed to a person, and careful about what it asserts. The board knows
/// two things — it meant to be this run's credential, and its helper was never
/// asked — and neither of them is "the agent did something wrong". What it
/// asks for is the one thing a reader can actually do: look at how the branch
/// got there before trusting the box's credential path again.
pub fn credential_comment(
    attempt_no: u64,
    branch: Option<&str>,
    reason: &str,
    log: &str,
) -> String {
    format!(
        "comet-board: attempt {attempt_no} finished with work on origin{}, but the board's \
         credential helper was never asked for it — {reason}. Whatever pushed, it was not the \
         installation token the board issues, so the guarantees in gh#68 (no token in argv, in \
         `.git/config`, or in the environment) were not the ones in force. Check how the branch \
         was pushed, and run `comet-board doctor` on the box before dispatching more work to it. \
         · log: {log}",
        branch.map(|b| format!(" ({b})")).unwrap_or_default(),
    )
}

/// The prompt queued into the chat of the agent that released this work.
///
/// Addressed to an agent, so it says what changed and what is actionable, and
/// nothing else — an orchestrator that gets this is spending a turn on it.
///
/// Both [`Signal`]s, not only the settle: a block is the state where *nothing*
/// happens until somebody acts, and the party who was waiting on that step is
/// the one who can act on it soonest. The body is the same one the orchestrator
/// would have got for the same event, because it is the same event; only the
/// lead line differs, and it differs because this reader did release the work.
pub fn dispatcher_message(task: &Task, attempt: &Attempt, signal: &Signal) -> String {
    match signal {
        Signal::Settled {
            outcome,
            evidence,
            pr_url,
            note,
        } => {
            let mut s = "comet-board: work you released has finished.\n\n".to_string();
            s.push_str(&settled_block(
                task,
                attempt,
                *outcome,
                *evidence,
                pr_url.as_deref(),
                note.as_deref(),
            ));
            s.push_str(
                "\nNo agent is working on it any more. `comet-board list --json` for the \
                 board's current view; if this was a step in something you are running, this \
                 is your cue to carry on.\n",
            );
            s
        }
        Signal::Blocked(why) => {
            let mut s = "comet-board: work you released is blocked.\n\n".to_string();
            s.push_str(&blocked_block(task, attempt, *why));
            s.push('\n');
            s.push_str(unsticks(*why));
            s
        }
    }
}

/// The identity block every settle notice shares: which task, which attempt,
/// how it ended, and the two artifacts a reader immediately wants.
///
/// One builder rather than one per audience, because the moment the dispatcher
/// and the orchestrator describe the same settle differently, the wording in
/// `docs/agent-conventions.md` stops being the contract for either of them.
fn settled_block(
    task: &Task,
    attempt: &Attempt,
    outcome: Outcome,
    evidence: Option<Evidence>,
    pr_url: Option<&str>,
    note: Option<&str>,
) -> String {
    let mut s = format!(
        "{}  attempt {} · {}{}",
        task_lines(task),
        attempt_no(task),
        outcome.as_str(),
        why(note),
    );
    if let Some(e) = evidence {
        s.push_str(&format!(" (settled on {})", e.as_str()));
    }
    s.push('\n');
    match pr_url {
        Some(url) => s.push_str(&format!("  pull request: {url}\n")),
        None => s.push_str("  no pull request was opened\n"),
    }
    if let Some(branch) = attempt.branch.as_deref() {
        s.push_str(&format!("  branch: {branch}\n"));
    }
    s
}

/// The identity block a block notice shares, for [`settled_block`]'s reason:
/// which task, which attempt, which of the two ways it stopped, and the chat —
/// which for a block is not context but the address an answer has to be typed
/// into.
fn blocked_block(task: &Task, attempt: &Attempt, why: Stopped) -> String {
    let mut s = format!(
        "{}  attempt {} · blocked ({})\n",
        task_lines(task),
        attempt_no(task),
        why.as_str(),
    );
    if let Some(chat) = attempt.pane_id.as_deref() {
        s.push_str(&format!("  chat: {chat}\n"));
    }
    s
}

/// What actually unsticks a block, for whichever agent was told about it.
///
/// Shared rather than written per audience: a dispatcher and an orchestrator
/// have the same two moves available to them here, and a board that described
/// them differently would be teaching two contracts for one state.
fn unsticks(why: Stopped) -> &'static str {
    match why {
        Stopped::Asking => {
            "It is waiting on an answer and will sit there until it gets one. Read the chat \
             and answer it, or `comet-board retry --task <id>` under a different model — \
             which discards the question.\n"
        }
        Stopped::Errored => {
            "The run died; the chat still holds the whole task, so this is a retry or a \
             cancel, not a lost attempt.\n"
        }
        Stopped::Unknown => {
            "Either it is waiting on an answer or its run died — the chat will say which. \
             Nothing further happens until somebody picks it up.\n"
        }
    }
}

/// A [`Signal::Settled::note`] as it reads beside the outcome it qualifies —
/// `failed — timed out after 2h 5m` — and nothing at all when there is none.
///
/// One renderer rather than one per audience, for [`settled_block`]'s reason:
/// the webhook's `text` and the two agent notices are describing one close.
fn why(note: Option<&str>) -> String {
    note.map(|n| format!(" — {n}")).unwrap_or_default()
}

/// Which task, in the two lines every notice opens with. Shared for the same
/// reason [`settled_block`] is: three events describing the same row three
/// slightly different ways is how a format nobody can parse gets written.
fn task_lines(task: &Task) -> String {
    format!("  {} · {}\n  {}\n", task.identifier, task.title, task.url)
}

/// Everything the pinned orchestrator can be told about, which is [`Signal`]
/// plus the one event no attempt's dispatcher could act on.
///
/// The duration cap's warning is here and not on `Signal` because the two are
/// answering different questions. `Signal` is "something happened to an attempt
/// and it is over or stuck", which is what the webhook and the issue comment
/// are about. A cap warning is the opposite — the attempt is *still running*,
/// and the only party who can do anything before the grace expires is an agent
/// that can go and look at it. Putting it on `Signal` would POST every operator
/// a notice about a run that is still fine.
pub enum Event<'a> {
    /// A settle, a block or an orphan — the same events audience 2 gets, and
    /// carried here only for the ones audience 2 could not be given.
    Signal(&'a Signal),
    /// An attempt has passed its route's `max_duration` and will be cancelled
    /// when the grace runs out (gh#70).
    CapWarning {
        age_secs: i64,
        cap_secs: u64,
        grace_secs: u64,
    },
}

/// The prompt queued into the pinned orchestrator's chat (gh#104).
///
/// Same body as the notice audience 2 gets — deliberately, so the one
/// description in `docs/agent-conventions.md` covers both — with two
/// differences that come from the orchestrator not being the party that
/// released the work:
///
/// - The lead line does not claim it released anything. Everything that
///   reaches the orchestrator since gh#165 was released by an operator, or by a
///   chat that is no longer there to be told.
/// - It names who *did* release it, when the board recorded anyone. That is the
///   fact an orchestrator acts on: a settle here with a `released by` line is
///   one whose dispatcher never heard about it, and picking that up is the job.
pub fn orchestrator_message(task: &Task, attempt: &Attempt, event: &Event) -> String {
    let mut s = match event {
        Event::Signal(Signal::Settled { .. }) => {
            "comet-board: work on the board has finished.\n\n".to_string()
        }
        Event::Signal(Signal::Blocked(_)) => {
            "comet-board: work on the board is blocked.\n\n".to_string()
        }
        Event::CapWarning { .. } => "comet-board: an attempt is past its time cap.\n\n".to_string(),
    };
    match event {
        Event::Signal(Signal::Settled {
            outcome,
            evidence,
            pr_url,
            note,
        }) => s.push_str(&settled_block(
            task,
            attempt,
            *outcome,
            *evidence,
            pr_url.as_deref(),
            note.as_deref(),
        )),
        Event::Signal(Signal::Blocked(why)) => s.push_str(&blocked_block(task, attempt, *why)),
        Event::CapWarning {
            age_secs,
            cap_secs,
            grace_secs,
        } => {
            s.push_str(&format!(
                "{}  attempt {} · running for {}, cap {}\n",
                task_lines(task),
                attempt_no(task),
                crate::overrun::human_secs(*age_secs),
                crate::overrun::human_secs(*cap_secs as i64),
            ));
            if let Some(chat) = attempt.pane_id.as_deref() {
                s.push_str(&format!("  chat: {chat}\n"));
            }
            s.push_str(&format!(
                "  will be cancelled in {}\n",
                crate::overrun::human_secs(*grace_secs as i64)
            ));
        }
    }
    if let Some(by) = released_by(attempt) {
        s.push_str(&format!("  released by: {by}\n"));
    }
    s.push('\n');
    s.push_str(match event {
        Event::Signal(Signal::Settled { .. }) => {
            "No agent is working on it any more. `comet-board list --json` for the board's \
             current view; review it, or carry on with whatever you were running.\n"
        }
        Event::Signal(Signal::Blocked(why)) => unsticks(*why),
        Event::CapWarning { .. } => {
            "The agent has been told to commit and open a pull request. Nothing is required \
             of you before the grace expires; after it, the attempt closes `failed` and is \
             yours to retry or leave.\n"
        }
    });
    s
}

/// Who released this attempt, as a line the orchestrator can read.
///
/// The task the releasing agent was itself running is the richest answer, its
/// chat the one that is always recorded, and the human's name attribution
/// beside the chat rather than instead of it. The name carries how well the box
/// knows it (gh#161): an agent deciding what to do about a run should be able
/// to see that "for ana@example.com" is something the edge verified and "for
/// ana@example.com (as claimed)" is something a frontend typed. `None` is the
/// operator at a keyboard, which needs no line: it is the default assumption.
fn released_by(attempt: &Attempt) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(task) = attempt.dispatched_by.as_deref() {
        parts.push(task.to_string());
    }
    if let Some(chat) = attempt.dispatched_by_pane.as_deref() {
        parts.push(format!("chat {chat}"));
    }
    if let Some(user) = attempt.dispatched_by_user.as_deref() {
        parts.push(format!(
            "for {}",
            comet_proto::view::board::dispatcher_label(user, attempt.dispatched_by_verified)
        ));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The JSON body POSTed to `[defaults] notify_webhook`.
///
/// Deliberately flat-ish and stable: `event` says which of the two things
/// happened, `text` renders on an endpoint that shows nothing else, and the
/// rest is there so a script can route on it without parsing prose.
pub fn webhook_payload(task: &Task, attempt: &Attempt, signal: &Signal, at: &str) -> Value {
    let mut body = json!({
        "event": signal.event(),
        "at": at,
        "text": summary(task, signal),
        "task": {
            "id": task.id,
            "identifier": task.identifier,
            "title": task.title,
            "url": task.url,
            "source": task.source.as_str(),
        },
        "attempt": {
            "number": attempt_no(task),
            "chat_id": attempt.pane_id,
            "workspace": attempt.workspace,
            "runtime": attempt.runtime,
            "branch": attempt.branch,
            "worktree": attempt.worktree,
        },
    });
    let map = body.as_object_mut().expect("a json object");
    match signal {
        Signal::Blocked(why) => {
            map.insert("reason".into(), json!(why.as_str()));
            // Which block this is, so a receiver can tell a re-block from a
            // duplicate delivery of the first one.
            map.insert("block".into(), json!(attempt.blocked_count.max(1)));
        }
        Signal::Settled {
            outcome,
            evidence,
            pr_url,
            note,
        } => {
            map.insert("outcome".into(), json!(outcome.as_str()));
            map.insert("evidence".into(), json!(evidence.map(Evidence::as_str)));
            map.insert("pr_url".into(), json!(pr_url));
            // Present and null on every settle rather than absent on most: a
            // receiver routing on `outcome` should not have to tell "the board
            // said nothing about why" from "this build did not have the field".
            map.insert("note".into(), json!(note));
        }
    }
    body
}

/// Attempt numbering, counted exactly as the dispatch and outcome comments
/// count it — how many attempts the task has, the current one included. An
/// ordinal is not stored on the attempt row, and inventing a second way to
/// derive it here would let one comment call a run "attempt 2" while the next
/// calls it "attempt 3".
fn attempt_no(task: &Task) -> usize {
    task.attempt_count().max(1)
}

/// Where a notification goes. A trait so the composition above is testable
/// without a listening socket — the board has exactly one implementation.
pub trait Webhook: Send + Sync {
    fn post(&self, url: &str, body: &Value) -> Result<()>;
}

/// The real one: a blocking POST with a short timeout, no retry.
pub struct HttpWebhook;

impl Webhook for HttpWebhook {
    fn post(&self, url: &str, body: &Value) -> Result<()> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(WEBHOOK_TIMEOUT_SECS))
            .build()
            .context("building the webhook client")?;
        let res = client
            .post(url)
            .json(body)
            .send()
            .with_context(|| format!("POST {url}"))?;
        let status = res.status();
        if !status.is_success() {
            let detail = res.text().unwrap_or_default();
            let detail = detail.chars().take(200).collect::<String>();
            anyhow::bail!("{url} answered {status}: {detail}");
        }
        Ok(())
    }
}

/// Is this URL something [`HttpWebhook`] can post to at all?
///
/// Checked by `doctor` rather than at delivery: a typo in a webhook URL is a
/// config mistake whose only symptom is silence, which is the exact failure
/// this whole module exists to remove.
pub fn webhook_url_problem(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() {
        return Some("empty".into());
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Some("not an http(s) URL".into());
    }
    match reqwest::Url::parse(url) {
        Ok(u) if u.host_str().is_some() => None,
        Ok(_) => Some("no host in the URL".into()),
        Err(e) => Some(e.to_string()),
    }
}

/// The host a URL points at, for a `doctor` line that names the destination
/// without printing a URL that is usually a secret (a Slack webhook URL *is*
/// the credential).
pub fn webhook_host(url: &str) -> String {
    reqwest::Url::parse(url.trim())
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| "?".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Source, UpstreamState};

    fn task() -> Task {
        Task {
            id: "linear:LIN-142".into(),
            source: Source::Linear,
            source_id: "uuid-1".into(),
            identifier: "LIN-142".into(),
            title: "Add retry".into(),
            body: None,
            url: "https://linear.app/x/LIN-142".into(),
            labels: vec![],
            source_state: None,
            linear_team: Some("LIN".into()),
            linear_project: None,
            upstream: UpstreamState::Started,
            state: crate::model::BoardState::Working,
            local_done: false,
            pr_url: None,
            pr_number: None,
            pr_open: false,
            pr_merged: false,
            pr_mergeable: None,
            pr_base_ref: None,
            pr_stack: None,
            pr_changes_requested: None,
            updated_at: crate::db::now(),
            synced_at: crate::db::now(),
            attempts: vec![],
        }
    }

    fn attempt() -> Attempt {
        Attempt {
            task_id: "linear:LIN-142".into(),
            pane_id: Some("chat-9".into()),
            worktree: Some("/tmp/wt".into()),
            branch: Some("board/lin-142".into()),
            started_at: crate::db::now(),
            dispatched_by_pane: Some("chat-parent".into()),
            ..crate::model::tests::blank_attempt()
        }
    }

    #[test]
    fn a_blocked_comment_names_itself_so_review_delivery_skips_it() {
        // The board's own comments land in the same thread a reviewer writes
        // in, and `review::is_actionable` recognises them by this prefix. A
        // blocked notice relayed back into the agent's chat as feedback would
        // be the board telling the agent it is blocked.
        let c = upstream_comment(1, Stopped::Asking, "/tmp/board.log");
        assert!(crate::review::is_the_boards_own(&c));
    }

    #[test]
    fn the_two_ways_of_blocking_read_differently() {
        let asking = upstream_comment(1, Stopped::Asking, "/l");
        let errored = upstream_comment(1, Stopped::Errored, "/l");
        assert!(asking.contains("waiting on an answer"));
        assert!(errored.contains("stopped with an error"));
        assert_ne!(asking, errored);
    }

    #[test]
    fn the_webhook_body_says_which_event_and_carries_a_text_line() {
        let t = task();
        let mut a = attempt();
        a.blocked_count = 2;
        let body = webhook_payload(
            &t,
            &a,
            &Signal::Blocked(Stopped::Errored),
            "2026-08-06T00:00:00Z",
        );
        assert_eq!(body["event"], "on_blocked");
        assert_eq!(body["reason"], "errored");
        assert_eq!(body["block"], 2);
        assert_eq!(body["task"]["identifier"], "LIN-142");
        assert!(
            body["text"].as_str().unwrap().contains("LIN-142"),
            "an endpoint that renders only `text` must still learn which task"
        );
        // No settle keys on a block: a receiver switching on `event` should
        // never find a half-filled outcome.
        assert!(body.get("outcome").is_none());
    }

    #[test]
    fn a_settled_body_carries_the_evidence_and_the_pull_request() {
        let body = webhook_payload(
            &task(),
            &attempt(),
            &Signal::Settled {
                outcome: Outcome::Done,
                evidence: Some(Evidence::PullRequest),
                pr_url: Some("https://github.com/o/r/pull/7".into()),
                note: None,
            },
            "2026-08-06T00:00:00Z",
        );
        assert_eq!(body["event"], "on_settled");
        assert_eq!(body["outcome"], "done");
        assert_eq!(body["evidence"], "PR");
        assert_eq!(body["pr_url"], "https://github.com/o/r/pull/7");
        assert!(body.get("reason").is_none());
    }

    #[test]
    fn the_dispatcher_message_says_what_is_actionable() {
        let m = dispatcher_message(
            &task(),
            &attempt(),
            &Signal::Settled {
                outcome: Outcome::Done,
                evidence: Some(Evidence::Commits),
                pr_url: None,
                note: None,
            },
        );
        assert!(m.contains("LIN-142"));
        assert!(m.contains("settled on commits"));
        assert!(
            m.contains("no pull request"),
            "an absent PR is a fact the dispatcher has to act on, not an omission"
        );
    }

    /// gh#165: a block is the event where nothing at all happens until somebody
    /// acts, and the agent whose plan that task was a step in is the one who
    /// can act on it soonest. It used to reach the dispatcher never.
    #[test]
    fn a_block_reaches_the_dispatcher_with_the_chat_to_answer_in() {
        let m = dispatcher_message(&task(), &attempt(), &Signal::Blocked(Stopped::Asking));
        assert!(m.contains("work you released is blocked"), "{m}");
        assert!(m.contains("LIN-142"), "{m}");
        assert!(m.contains("chat: chat-9"), "where to answer it: {m}");
        assert!(m.contains("waiting on an answer"), "{m}");

        let died = dispatcher_message(&task(), &attempt(), &Signal::Blocked(Stopped::Errored));
        assert!(died.contains("The run died"), "{died}");
    }

    /// The block's body and its advice are shared with the orchestrator's copy
    /// for [`settled_block`]'s reason: two audiences describing one state two
    /// ways is two contracts for it.
    #[test]
    fn the_two_audiences_describe_a_block_the_same_way() {
        let (t, a) = (task(), attempt());
        let signal = Signal::Blocked(Stopped::Unknown);
        let to_dispatcher = dispatcher_message(&t, &a, &signal);
        let to_orchestrator = orchestrator_message(&t, &a, &Event::Signal(&signal));
        let block = blocked_block(&t, &a, Stopped::Unknown);
        assert!(to_dispatcher.contains(&block));
        assert!(to_orchestrator.contains(&block));
        assert!(to_dispatcher.contains(unsticks(Stopped::Unknown)));
        assert!(to_orchestrator.contains(unsticks(Stopped::Unknown)));
        // ...and only the dispatcher's claims it released anything.
        assert!(to_dispatcher.contains("work you released"));
        assert!(!to_orchestrator.contains("work you released"));
    }

    /// The point of sharing [`settled_block`]: whatever else the two notices
    /// say, they describe the settle itself identically — which is what lets
    /// `docs/agent-conventions.md` document it once.
    #[test]
    fn the_orchestrator_and_the_dispatcher_describe_a_settle_the_same_way() {
        let (t, a) = (task(), attempt());
        let signal = Signal::Settled {
            outcome: Outcome::Done,
            evidence: Some(Evidence::PullRequest),
            pr_url: Some("https://github.com/o/r/pull/7".into()),
            note: None,
        };
        let to_dispatcher = dispatcher_message(&t, &a, &signal);
        let to_orchestrator = orchestrator_message(&t, &a, &Event::Signal(&signal));
        let block = settled_block(
            &t,
            &a,
            Outcome::Done,
            Some(Evidence::PullRequest),
            Some("https://github.com/o/r/pull/7"),
            None,
        );
        assert!(to_dispatcher.contains(&block));
        assert!(to_orchestrator.contains(&block));
        // ...and only the orchestrator's is careful not to claim it released it.
        assert!(to_dispatcher.contains("work you released"));
        assert!(!to_orchestrator.contains("work you released"));
    }

    // ---- the print that tells a repeat from a second settle (gh#356) -----

    fn settled(pr: Option<&str>) -> Signal {
        Signal::Settled {
            outcome: Outcome::Done,
            evidence: Some(Evidence::PullRequest),
            pr_url: pr.map(str::to_string),
            note: None,
        }
    }

    /// The bug, at the level the decision is made: the pull request is still
    /// open, so the attempt settles again the moment its chat stops — and every
    /// one of those closes reads identically, because it is one close.
    #[test]
    fn a_settle_repeated_on_an_unchanged_attempt_prints_the_same() {
        let s = settled(Some("https://github.com/o/r/pull/7"));
        assert_eq!(
            s.settle_print(Some("31d8fc5")),
            s.settle_print(Some("31d8fc5"))
        );
    }

    /// Each of the three things the dispatcher can act on, one at a time. A
    /// print that missed any of them would suppress a settle somebody is
    /// waiting on, which is the worse half of this trade.
    #[test]
    fn a_new_commit_a_new_pull_request_or_a_new_outcome_all_print_differently() {
        let s = settled(Some("https://github.com/o/r/pull/7"));
        let base = s.settle_print(Some("31d8fc5"));
        assert_ne!(base, s.settle_print(Some("1fbcfb4")), "a new commit");
        assert_ne!(
            base,
            settled(Some("https://github.com/o/r/pull/8")).settle_print(Some("31d8fc5")),
            "a new pull request"
        );
        assert_ne!(
            base,
            Signal::Settled {
                outcome: Outcome::Cancelled,
                evidence: None,
                pr_url: Some("https://github.com/o/r/pull/7".into()),
                note: Some("cancelled from the panel".into()),
            }
            .settle_print(Some("31d8fc5")),
            "a different close"
        );
    }

    /// An unreadable checkout must not make two different settles look alike.
    /// `None` is one value among many, not a wildcard — so it prints as itself,
    /// and differs from every branch head there is.
    #[test]
    fn an_unknown_head_is_a_value_not_a_wildcard() {
        let s = settled(Some("https://github.com/o/r/pull/7"));
        assert_ne!(s.settle_print(None), s.settle_print(Some("31d8fc5")));
        assert_eq!(s.settle_print(None), s.settle_print(None));
    }

    /// A block has no print: it is told once per block by `blocked_count`,
    /// which counts a state rather than an event, and a second mark for the
    /// same thing is a second way for the two to disagree.
    #[test]
    fn a_block_has_no_print_of_its_own() {
        assert!(
            Signal::Blocked(Stopped::Asking)
                .settle_print(Some("31d8fc5"))
                .is_none()
        );
    }

    /// Most of what reaches the orchestrator is somebody else's dispatch, and
    /// which somebody is the difference between "a step in my own plan landed"
    /// and "I have just been handed something".
    #[test]
    fn the_orchestrator_is_told_who_released_the_work() {
        let mut a = attempt();
        a.dispatched_by = Some("linear:LIN-9".into());
        a.dispatched_by_user = Some("brede@tally.no".into());
        let signal = Signal::Blocked(Stopped::Asking);
        let m = orchestrator_message(&task(), &a, &Event::Signal(&signal));
        assert!(m.contains("released by: linear:LIN-9 · chat chat-parent · for brede@tally.no"));
        // An operator's dispatch records nobody, and a line saying so would be
        // noise on the common case.
        let mut bare = attempt();
        bare.dispatched_by_pane = None;
        assert!(
            !orchestrator_message(&task(), &bare, &Event::Signal(&signal)).contains("released by")
        );
    }

    /// A block reaching the orchestrator has to say what unsticks it — it is
    /// the one event where nothing at all happens until somebody acts.
    #[test]
    fn a_block_tells_the_orchestrator_what_to_do_about_it() {
        let asking = Signal::Blocked(Stopped::Asking);
        let errored = Signal::Blocked(Stopped::Errored);
        let a = orchestrator_message(&task(), &attempt(), &Event::Signal(&asking));
        let e = orchestrator_message(&task(), &attempt(), &Event::Signal(&errored));
        assert!(a.contains("waiting on an answer"));
        assert!(a.contains("comet-board retry"));
        assert!(e.contains("The run died"));
        // The chat is the address the answer has to be typed into.
        assert!(a.contains("chat: chat-9"));
    }

    /// The cap warning is the one notice about a run that is still going, so it
    /// has to say how long is left and that nothing is required of the reader.
    #[test]
    fn a_cap_warning_names_the_remaining_grace() {
        let m = orchestrator_message(
            &task(),
            &attempt(),
            &Event::CapWarning {
                age_secs: 7_500,
                cap_secs: 7_200,
                grace_secs: 600,
            },
        );
        assert!(m.contains("running for 2h 5m, cap 2h"));
        assert!(m.contains("will be cancelled in 10m"));
        assert!(m.contains("Nothing is required of you"));
    }

    #[test]
    fn a_typo_in_the_webhook_url_is_caught_by_inspection() {
        assert_eq!(webhook_url_problem("https://hooks.example.com/abc"), None);
        assert!(webhook_url_problem("").is_some());
        assert!(webhook_url_problem("hooks.example.com/abc").is_some());
        assert!(webhook_url_problem("https://").is_some());
        assert_eq!(
            webhook_host("https://hooks.slack.com/x/y"),
            "hooks.slack.com"
        );
    }
}
