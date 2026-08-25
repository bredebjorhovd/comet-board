//! Task/attempt types and the state-derivation matrix.
//!
//! Board state is **derived**, never trusted from one side alone: upstream state
//! (GitHub) plus the live attempt (a herdr pane) produce the board state.
//! `derive_state` is the single place that decision is made; it is pure so the
//! matrix can be tested exhaustively.

use serde::{Deserialize, Serialize};

/// Board-level task state. Moved to proto so the viewports can share the
/// glyph/section vocabulary without depending on this crate; the semantics
/// (and the derivation matrix below) are the board's.
pub use comet_proto::view::board::BoardState;

/// Upstream state, normalized. GitHub has only open/closed; `Started` survives
/// on rows written before the board stopped syncing sources that carried a
/// started-type workflow state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpstreamState {
    /// GitHub open with no started marker.
    Unstarted,
    /// A started-type workflow state, on legacy rows only.
    Started,
    /// GitHub closed.
    Terminal,
    /// The issue itself is no longer there: a full sweep of the source stopped
    /// returning it. Never set by a poll — only by reaping, and only on a task
    /// that has attempts worth keeping (impl spec §4, AGE-6).
    Gone,
}

impl UpstreamState {
    pub fn as_str(self) -> &'static str {
        match self {
            UpstreamState::Unstarted => "unstarted",
            UpstreamState::Started => "started",
            UpstreamState::Terminal => "terminal",
            UpstreamState::Gone => "gone",
        }
    }

    pub fn parse(s: &str) -> Option<UpstreamState> {
        Some(match s {
            "unstarted" => UpstreamState::Unstarted,
            "started" => UpstreamState::Started,
            "terminal" => UpstreamState::Terminal,
            "gone" => UpstreamState::Gone,
            _ => return None,
        })
    }

    /// Nothing more is coming from upstream: the issue is closed, or the issue
    /// is not there at all. Both end the task; neither can be written back to.
    pub fn is_final(self) -> bool {
        matches!(self, UpstreamState::Terminal | UpstreamState::Gone)
    }
}

/// Live agent status as reported by herdr, plus `Missing` for a pane herdr no
/// longer knows about. Mirrors herdr's `agent_status` values exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Working,
    Blocked,
    /// herdr: ready for input, tab already seen.
    Idle,
    /// herdr: same underlying idle state after unseen background work finished.
    Done,
    /// An agent is present but herdr cannot classify it. Explicitly NOT proof of
    /// successful completion.
    Unknown,
    /// The pane is gone, or herdr does not know this pane id.
    Missing,
}

impl AgentStatus {
    pub fn parse(s: &str) -> AgentStatus {
        match s {
            "working" => AgentStatus::Working,
            "blocked" => AgentStatus::Blocked,
            "idle" => AgentStatus::Idle,
            "done" => AgentStatus::Done,
            "missing" => AgentStatus::Missing,
            _ => AgentStatus::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AgentStatus::Working => "working",
            AgentStatus::Blocked => "blocked",
            AgentStatus::Idle => "idle",
            AgentStatus::Done => "done",
            AgentStatus::Unknown => "unknown",
            AgentStatus::Missing => "missing",
        }
    }
}

/// Terminal outcome of an attempt. `None` in the DB means the attempt is live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Done,
    Failed,
    Cancelled,
    /// The pane vanished without completing.
    Orphaned,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Done => "done",
            Outcome::Failed => "failed",
            Outcome::Cancelled => "cancelled",
            Outcome::Orphaned => "orphaned",
        }
    }

    pub fn parse(s: &str) -> Option<Outcome> {
        Some(match s {
            "done" => Outcome::Done,
            "failed" => Outcome::Failed,
            "cancelled" | "canceled" => Outcome::Cancelled,
            "orphaned" => Outcome::Orphaned,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// Legacy only (gh#471). Linear is no longer a connected source: nothing
    /// polls it, writes back to it, or creates issues on it. The variant stays
    /// so rows written while it was connected still load and render as
    /// history; no poll refreshes them, so they only leave the board by being
    /// marked done or reaped.
    Linear,
    Github,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Linear => "linear",
            Source::Github => "github",
        }
    }

    pub fn parse(s: &str) -> Option<Source> {
        Some(match s {
            "linear" => Source::Linear,
            "github" => Source::Github,
            _ => return None,
        })
    }
}

/// Everything `derive_state` is allowed to look at. Keeping this a plain struct
/// (rather than reaching into a `Task`) is what makes the matrix testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Derivation {
    pub upstream: UpstreamState,
    /// Status of the live attempt's agent, if an attempt is open.
    pub live: Option<AgentStatus>,
    /// Outcome of the most recent *closed* attempt, if any.
    pub last_outcome: Option<Outcome>,
    /// A pull request is linked to this task and still open.
    pub open_pr: bool,
    /// A layer below this one has been asked to change (gh#289), so this row's
    /// pull request needs an ordered rebase and is not reviewable in good faith.
    ///
    /// The one fact in here that is not about this task at all. It is here for
    /// the reason the stack changed what `clean` means: a `review` row is a
    /// claim somebody can act on, and acting on this one — reading the diff,
    /// approving it — is work that the resulting upstack replay invalidates.
    pub changes_below: bool,
    /// Operator pressed `d mark done`. Survives re-derivation until the task is
    /// re-dispatched; without it, a derived state would silently undo the key.
    pub local_done: bool,
}

impl Default for Derivation {
    fn default() -> Self {
        Derivation {
            upstream: UpstreamState::Unstarted,
            live: None,
            last_outcome: None,
            open_pr: false,
            changes_below: false,
            local_done: false,
        }
    }
}

/// The state-derivation matrix (impl spec §3/§6).
///
/// Rule order is load-bearing; each arm documents why it outranks the next.
pub fn derive_state(d: Derivation) -> BoardState {
    // 1. An explicit `d mark done` is the operator overruling derivation. It has
    //    to outrank everything, otherwise the next poll recomputes the upstream
    //    state and the key undoes itself.
    if d.local_done {
        return BoardState::Done;
    }

    // 2. Closed upstream is the end of the story, and so is deleted upstream —
    //    `gone` is a task whose issue the source stopped returning. This
    //    outranks a live attempt: if the issue was closed while an agent worked,
    //    the work is moot and the operator should see it leave the queue.
    if d.upstream.is_final() {
        return BoardState::Done;
    }

    // 3. A live attempt outranks any non-terminal upstream state — in particular
    //    it outranks upstream `ready`, which is the spec's explicit rule.
    if let Some(agent) = d.live {
        return match agent {
            AgentStatus::Blocked => BoardState::Blocked,
            AgentStatus::Working => BoardState::Working,
            // The pane is gone but the attempt was never closed.
            AgentStatus::Missing => BoardState::Failed,
            // Agent settled. A PR is the only *explicit* done detection we have;
            // without one the agent may simply be between turns, so the task
            // stays `working` and the UI renders a dim `idle` marker instead of
            // inventing a state.
            AgentStatus::Idle | AgentStatus::Done | AgentStatus::Unknown => {
                if d.open_pr {
                    reviewable(d)
                } else {
                    BoardState::Working
                }
            }
        };
    }

    // 4. No live attempt. A PR outranks the closed attempt's outcome — the work
    //    landed somewhere reviewable regardless of how the pane ended.
    if d.open_pr {
        return reviewable(d);
    }

    match d.last_outcome {
        // Both mean "needs you". Separated from `blocked` by glyph and section.
        Some(Outcome::Failed) | Some(Outcome::Orphaned) => BoardState::Failed,
        // The agent finished and nobody has looked. herdr would call this
        // `done`; on the board that is `review`.
        Some(Outcome::Done) => BoardState::Review,
        // Cancelling ends the attempt, not the issue. This is design-spec gap
        // #5: upstream sits in a `started`-type state with no live attempt and
        // no PR, and it must derive to `ready` — the issue is still owed.
        Some(Outcome::Cancelled) | None => BoardState::Ready,
    }
}

/// What an open pull request derives to: `review`, unless a layer below it has
/// been asked to change (gh#289).
///
/// `review` on the board means one thing — finished work, waiting on a human to
/// read it. A layer whose parent is about to be rewritten is not that: the
/// direct child must replay this path onto a different base, so the diff being
/// read will not be the diff that lands. Worse, if somebody approves it, the
/// approval outlives the diff it was given to.
///
/// So the row comes out of the review section and says it is waiting — `blocked`
/// rather than a seventh state, because `blocked` is already the board's word for
/// work that has stopped short of an answer and needs somebody. It goes back to
/// `review` by itself: the fact is derived, and it stops being true when the
/// layer below is approved, merged, or closed.
///
/// **Only a settled layer.** A child still `working` never reaches here — rule 3
/// answers `working` for it — because its run may produce work that survives the
/// ordered rebase, and killing an in-flight agent to save it a rebase spends a
/// context to save a `git rebase`.
fn reviewable(d: Derivation) -> BoardState {
    if d.changes_below {
        BoardState::Blocked
    } else {
        BoardState::Review
    }
}

/// A pull request's place in a GitHub stack.
///
/// GitHub's stacked pull requests are in public preview, and the `stack` object
/// it hangs off a pull request is absent on every standalone PR — so this is
/// `Option` at every layer that carries it, and the parser that builds it
/// treats a shape it does not recognise as "not stacked" rather than as an
/// error. A preview that renames a field must cost the board its stack view,
/// never its sync (gh#282).
///
/// Stored flat on the task row rather than as a JSON blob: these four facts are
/// what sibling grouping, retarget detection and parent-feedback fan-out all
/// read, and a column is greppable from `sqlite3 board.db` the way every other
/// `pr_*` field already is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrStack {
    /// Identifies the stack — every PR in one carries the same value. This is
    /// what siblings are grouped by, so it is the one field required to call a
    /// PR stacked at all.
    pub number: i64,
    /// How many pull requests the stack holds, per GitHub.
    pub size: Option<i64>,
    /// This pull request's place in it, counting from the bottom.
    pub position: Option<i64>,
    /// The branch the *stack* targets — the bottom PR's base, which is where
    /// the whole stack eventually lands. Distinct from [`Task::pr_base_ref`],
    /// which for any PR above the bottom is the branch below it.
    pub base_ref: Option<String>,
}

#[derive(Debug, Clone)]
// `source_state`, `updated_at` and `synced_at` are stored columns required by
// impl spec §3 and are read by `sqlite3 board.db` when debugging; the UI derives
// what it shows rather than reading them.
#[allow(dead_code)]
pub struct Task {
    pub id: String,
    pub source: Source,
    pub source_id: String,
    pub identifier: String,
    pub title: String,
    pub body: Option<String>,
    pub url: String,
    pub labels: Vec<String>,
    pub state: BoardState,
    pub source_state: Option<String>,
    pub upstream: UpstreamState,
    pub local_done: bool,
    pub pr_url: Option<String>,
    pub pr_number: Option<i64>,
    pub pr_open: bool,
    /// Merged, rather than closed without merging.
    pub pr_merged: bool,
    /// GitHub's `mergeable_state` — `behind` and `dirty` are the ones that
    /// matter when several branches are in flight at once.
    pub pr_mergeable: Option<String>,
    /// The branch the PR merges *into*. Half of the topology: `head_ref` said
    /// where the work is, and the board never asked where it lands, so a PR
    /// retargeted when its parent merged looked exactly like one that had not
    /// moved (gh#282).
    pub pr_base_ref: Option<String>,
    /// The branch the PR's work is on. The other half of the same fact, and the
    /// only record of it for a pull request the board never dispatched (gh#344)
    /// — a dispatched one is answered by its attempt's own `branch`, which is
    /// what [`crate::review::authoring_attempt`] matches on.
    pub pr_head_ref: Option<String>,
    /// Set only when GitHub says this PR is part of a stack.
    pub pr_stack: Option<PrStack>,
    /// The review that last asked this pull request to change, and which nothing
    /// has withdrawn since (gh#289). `None` is "nothing outstanding" — never
    /// asked, or asked and since approved or merged.
    ///
    /// A fact about this row that the rows *above* it read: when this branch
    /// pushes its fix, its direct child must launch one ordered upstack rebase
    /// that carries every layer above it forward. The id rather than a flag,
    /// because the fan-out is watermarked against it — one review reaches each
    /// dependent once.
    pub pr_changes_requested: Option<i64>,
    pub updated_at: String,
    pub synced_at: String,
    /// Populated by the read path, not stored on the row.
    pub attempts: Vec<Attempt>,
}

impl Task {
    /// The live attempt, if one is open.
    pub fn live_attempt(&self) -> Option<&Attempt> {
        self.attempts.iter().find(|a| a.outcome.is_none())
    }

    /// The most recent attempt that has ended, if any.
    ///
    /// The single definition of "how did the last go", shared by `derive_state`
    /// and by `list --json`. A parent agent polling for its child reads the same
    /// fact the board derives from, so the two can never disagree about whether
    /// a `ready` row was cancelled or never ran.
    pub fn last_closed_attempt(&self) -> Option<&Attempt> {
        self.attempts.iter().rev().find(|a| a.outcome.is_some())
    }

    /// Attempt count, used for the `attempt <n>` writeback comment.
    pub fn attempt_count(&self) -> usize {
        self.attempts.len()
    }
}

/// The `owner/repo` (and bare-name) parsers a GitHub task id answers to.
///
/// GitHub numbers issues per repository, so the repo is the half of a task id
/// that makes it unique. Anything keyed on the *identifier* alone — `gh#2`, and
/// the `board/gh-2` branch that used to come from it — is keyed on something
/// two repos can both answer to.
///
/// The parsers themselves moved to proto (gh#125): the viewports key each row's
/// leading token on the repo name now, and two parsers of one id format is one
/// too many. Re-exported here because board-side code keys branches and panes
/// on them.
pub use comet_proto::view::board::{gh_repo, gh_repo_name};

/// The `owner/repo` a pull request URL names —
/// `https://github.com/Florin-AS/tally/pull/507` → `Florin-AS/tally`.
///
/// The answer for a row whose *id* names no repository — a legacy row from a
/// source that numbered issues per team rather than per repo, with a linked
/// pull request. Everything that scopes work to a repository — merging
/// it, grouping its stack (gh#283) — needs one, and the pull request's own URL
/// is the only place such a row carries it.
pub fn pr_repo(url: &str) -> Option<String> {
    let rest = url.split("github.com/").nth(1)?;
    let mut parts = rest.split('/');
    let owner = parts.next().filter(|p| !p.is_empty())?;
    let repo = parts.next().filter(|p| !p.is_empty())?;
    Some(format!("{owner}/{repo}"))
}

/// The number the same URL names — `…/tally/pull/507` → `507`.
///
/// [`pr_repo`]'s other half, for the one surface that holds a review rather
/// than a row (gh#408): an `AttemptReview` carries the pull request as its URL,
/// and a merge confirmation has to give the address as a number a reader can
/// match against GitHub.
pub fn pr_url_number(url: &str) -> Option<i64> {
    let rest = url.split("github.com/").nth(1)?;
    let mut parts = rest.split('/');
    let _owner = parts.next().filter(|p| !p.is_empty())?;
    let _repo = parts.next().filter(|p| !p.is_empty())?;
    if parts.next()? != "pull" {
        return None;
    }
    parts.next()?.parse().ok()
}

/// The issue number a GitHub task id closes with — `gh:owner/repo#932` → 932.
///
/// The number is the tail of the *id* and of nowhere else on the row:
/// `source_id` holds GitHub's node id (gh#471-era rows predate the repo in the
/// id, but every id that parses here has the number in it), and the identifier
/// (`gh#932`) is its short form. Splits on `#` only — the `!` form
/// (`gh:owner/repo!508`) is a pull request's own row, and a pull request is
/// not something another pull request closes.
pub fn gh_number(task_id: &str) -> Option<i64> {
    task_id.strip_prefix("gh:")?.split('#').nth(1)?.parse().ok()
}

#[derive(Debug, Clone)]
pub struct Attempt {
    pub id: i64,
    pub task_id: String,
    pub pane_id: Option<String>,
    pub workspace: String,
    pub runtime: String,
    pub worktree: Option<String>,
    /// Whether Board created and therefore owns the checkout/chat lifecycle.
    /// False for a PR adopted from an ordinary Comet chat: review may read it,
    /// but retention must never delete its checkout or archive its chat.
    pub board_managed: bool,
    /// The repo the worktree was cut from (gh#72) — recorded at dispatch,
    /// because reclaiming the checkout's branch afterwards is work that happens
    /// in the repo and not in the (by then deleted) worktree. `None` on every
    /// attempt made before this was recorded.
    pub repo_path: Option<String>,
    pub branch: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub outcome: Option<Outcome>,
    /// The agent-account slot this attempt ran under — whose Claude/Codex
    /// subscription it spent (gh#59). `None` is the device's own CLI login,
    /// and every attempt made before accounts could be chosen.
    pub account: Option<String>,
    /// Consecutive reconciliation ticks where herdr did not know this pane.
    /// Orphaning waits for 2 to avoid flapping during a live handoff.
    pub missing_ticks: i64,
    /// Live agent status, filled in by reconciliation; not a stored column.
    pub agent_status: Option<AgentStatus>,
    /// Task id of the parent that released this one, when the board dispatched
    /// that parent too. `None` on its own does **not** mean the operator: most
    /// dispatching agents sit in a pane the board never dispatched and so have
    /// no task id at all — see `dispatched_by_pane` and [`Dispatcher`].
    pub dispatched_by: Option<String>,
    /// The pane the dispatch ran from, when an agent was in it. This is the
    /// column that answers "released by" for the common case: every command
    /// carries `HERDR_PANE_ID`, whether or not the board started the pane.
    pub dispatched_by_pane: Option<String>,
    /// The commit this attempt's branch was cut from. Commits *after* it are
    /// the agent's output; commits before it are the operator's own unpushed
    /// work, which must not be mistaken for the agent having finished.
    pub base_sha: Option<String>,
    /// The attempt this one's branch was cut from, when it was dispatched onto
    /// a sibling rather than onto trunk (gh#285) — the stacking edge itself.
    ///
    /// An attempt id and not the base branch string, because the branch is the
    /// part that stops being true: a parent that merges has its branch deleted,
    /// and a child looking for its parent by branch equality then finds
    /// nothing. What the dependents want to know is *which run* this was cut
    /// from — collecting a parent's checkout while a child still builds on it,
    /// and fanning review feedback down a chain, are both questions about the
    /// attempt.
    ///
    /// `None` is an ordinary dispatch off the route's `base`, which is nearly
    /// all of them, and every attempt from before stacking existed.
    pub stacked_on: Option<i64>,
    /// Whether herdr has ever reported this attempt's pane as `working`. An
    /// agent that was never seen working cannot have finished, which is what
    /// stops a freshly-started `idle` agent from being reaped before its
    /// prompt has even been delivered.
    pub saw_working: bool,
    /// When this attempt *first* looked finished on commits alone, and still
    /// does. Settling waits [`crate::settled::SETTLE_SECS`] from here, because
    /// `idle` is a screen classification that flaps while an agent works, not a
    /// stable fact — and counting samples instead of seconds was no wait at all
    /// once reconciliation began running on demand (gh#18, gh#34). Cleared the
    /// moment a sample says `working`; a pull request bypasses it entirely.
    pub settled_at: Option<String>,
    /// How many times the board closed this attempt and then found its pane
    /// working again (gh#34). Not a retry: nobody dispatched anything, so it is
    /// counted here on the one attempt rather than by inventing a second one.
    pub reopened: i64,
    /// Digest of this pane's screen the last time it changed, and when that was
    /// — see [`crate::screen::fingerprint`]. Together they answer the one
    /// question a detection manifest cannot: whether the spinner it matched is
    /// live, or a line left in scrollback by a turn that died (gh#32).
    pub screen_print: Option<String>,
    pub screen_at: Option<String>,
    /// How many times the board has restarted this attempt's run in its own
    /// chat, after finding the chat alive with no run in it (gh#390).
    ///
    /// **Not a retry and not [`reopened`](Self::reopened).** A retry is a fresh
    /// attempt in a fresh chat; a re-open is the board taking back a settle it
    /// got wrong. This is the same attempt, the same chat and the same branch,
    /// picked up after something outside the run killed it — an engine restart
    /// above all, which used to close every live attempt on the box as
    /// `orphaned`. Capped at [`crate::runs::MAX_RESUMES`], because a run that
    /// cannot survive three starts is a fact about the box.
    pub resumes: i64,
    /// How many times the board has typed a "carry on" into this pane for the
    /// stall it is currently in, and when the last one went (gh#40). Capped at
    /// [`crate::nudge::MAX_NUDGES`], and cleared once the pane has been moving
    /// long enough for the movement to be the agent rather than the echo of the
    /// nudge itself — see [`crate::nudge::recovered`].
    pub nudges: i64,
    pub nudged_at: Option<String>,
    /// How many times this attempt has *entered* blocked — the agent stopped
    /// to ask, or its run died (gh#71). Not a tick count: it moves on the
    /// transition only, which is what lets the upstream notice be one comment
    /// per block rather than one per attempt or one every thirty seconds.
    pub blocked_count: i64,
    /// When the board told this attempt's chat it had run past its route's
    /// `max_duration` (gh#70). `None` is "not warned" — every attempt gets one
    /// warning and a grace period before the cap closes it, so this is also
    /// what says whether the grace clock has started. Cleared by a re-open.
    pub overrun_warned_at: Option<String>,
    /// When this attempt's checkout first became nobody's — the attempt closed
    /// and its task gone from the board (gh#72). The retention window is
    /// measured from here rather than from `ended_at`, so a pull request that
    /// sits in review for a fortnight still leaves a full window to look at the
    /// checkout after it merges. Cleared if the task comes back to life.
    pub collectable_at: Option<String>,
    /// When the checkout and its local branch were reclaimed (gh#72). Non-`None`
    /// means `worktree` names a path that is no longer on disk.
    pub collected_at: Option<String>,
    /// When the build output inside this attempt's checkout became nobody's
    /// (gh#186) — the attempt closed, with no live attempt on the task. A much
    /// earlier moment than [`collectable_at`](Self::collectable_at), and on
    /// purpose: it does not wait for the task to leave the board, because
    /// nothing reads `target/` after the run ends.
    pub cache_sweepable_at: Option<String>,
    /// When the board last deleted the build output inside this checkout
    /// (gh#186).
    ///
    /// **Not [`collected_at`](Self::collected_at), and never written as it**: the
    /// checkout is still there, still on its branch, still the directory review
    /// delivery would resume an agent in. All that is gone is a cache the next
    /// build rewrites. Cleared when a re-opened attempt starts building again, so
    /// the output of that build is swept too.
    pub cache_swept_at: Option<String>,
    /// The device the dispatch was issued from, as its frontend reported it
    /// (gh#74). `None` where nobody said — a `comet-board` run on the box, and
    /// every attempt from before the frontends sent it.
    pub dispatched_by_device: Option<String>,
    /// Which *human* released the work — an email where there is one, else the
    /// user id (gh#74).
    pub dispatched_by_user: Option<String>,
    /// Did the edge verify that human, or did a frontend simply say so?
    /// (gh#161.)
    ///
    /// True means the relay stamped an identity onto the frame and this box
    /// resolved it to the address above — the record a `require-own` refusal is
    /// made of. False means a claim: a dispatch issued on the box itself, a
    /// verified caller nobody could put a name to, and every attempt from
    /// before this column existed. Kept beside the name rather than folded into
    /// it because "we know who this was" and "they told us" must not read the
    /// same, and because neither is authority: what a run may spend is still
    /// the explicit `account` (gh#59).
    pub dispatched_by_verified: bool,
    /// Whose subscription this attempt actually spends, as an email (gh#101) —
    /// the `account` slot's login, or the box's own CLI login when the dispatch
    /// named no slot.
    ///
    /// Resolved once, at dispatch, and recorded here rather than looked up from
    /// `account` on demand: a slot id means nothing to a reader who has not
    /// saved that login, the box's own login can be switched under a run that
    /// is still going, and the point of the record is what the attempt *did*
    /// spend. `None` on attempts from before this existed, and on a box that
    /// could not name the login at all.
    pub billed_to: Option<String>,
    /// When this attempt's *chat* first became nobody's (gh#139) — the same
    /// moment [`collectable_at`](Self::collectable_at) records for its
    /// checkout, kept separately because the two windows are configured
    /// separately and a box may want one without the other.
    ///
    /// Cleared if the task comes back to life, so the next window starts whole.
    pub chat_archivable_at: Option<String>,
    /// When the board archived this attempt's chat off its space's shelf
    /// (gh#139). Non-`None` means the chat is *archived*, not gone: the
    /// transcript is intact and Settings → Archived puts it back. Cleared when
    /// a wrongly-settled attempt is re-opened, which un-archives the chat in
    /// the same motion.
    pub chat_archived_at: Option<String>,
    /// What this attempt spent, summed off its chat's run journal (gh#151).
    ///
    /// `None` is **"nothing was reported"**, not zero: every attempt made
    /// before the board recorded tokens has it, as does any run on a harness
    /// that meters nothing. The stats page keeps that distinction all the way
    /// to the pixel — a blank, never a 0, because a zero reads as free work —
    /// and says what share of a window it could account for.
    ///
    /// Refreshed on every reconcile while the attempt is live, so one that is
    /// cancelled or orphaned keeps what it had spent up to the last tick.
    pub tokens: Option<comet_proto::TokenUsage>,
    /// Exact per-model slices reported beside [`tokens`](Self::tokens)
    /// (gh#426). `None` means this attempt's harness/journal exposed no model
    /// attribution; it is not an empty or zero-valued breakdown.
    pub token_models: Option<Vec<comet_proto::ModelTokenUsage>>,
    /// Assistant-step usage attributed to the main agent or a named subagent,
    /// per model (gh#426). Additive metadata beside `tokens`, never another
    /// total to sum into it. `None` means attribution was not reported.
    pub token_agents: Option<Vec<comet_proto::AgentTokenUsage>>,
    /// How full this attempt's context window was when its harness last said
    /// (gh#271).
    ///
    /// The other meter, and a different kind of number from
    /// [`tokens`](Self::tokens): spend accumulates, fullness is a level, so
    /// this is overwritten by each fresh reading rather than added to. What it
    /// answers is the question the spend column cannot — whether the agent is
    /// about to have the context it is working from compacted out from under
    /// it, which is a live-attempt fact, not a bill.
    ///
    /// `None` is "nothing reported", exactly as for tokens: a harness that
    /// meters no window (opencode), a Claude CLI too old to answer, every
    /// attempt from before this existed. Rendered as a blank, never as 0% —
    /// which would read as an empty context rather than a silent one.
    ///
    /// Refreshed on every reconcile while the attempt is live, so an attempt
    /// that is cancelled or orphaned keeps the last level it was seen at.
    pub context: Option<comet_proto::ContextUsage>,
    /// The model the harness said it was running, recorded beside the tokens
    /// (gh#151). Not the route's override — see
    /// [`crate::runtime::RunTokens`] for why that is nearly always `None` and
    /// this nearly always is not.
    pub model: Option<String>,
    /// What the agent says it did, file-anchored (§gh#183).
    ///
    /// On the attempt and not on the task because a retry is a different agent
    /// on a different branch making different claims, and because the review
    /// happens after the chat is archived — the conversation that produced them
    /// is not somewhere the board can go back and read.
    ///
    /// Empty is the common case and the honest one: an attempt that never
    /// answered the contract claimed nothing, and
    /// [`claims_at`](Self::claims_at) is what tells that apart from an agent
    /// that submitted an empty set.
    pub claims: Vec<crate::claims::Claim>,
    /// When the claims above were submitted. `None` with an empty list means
    /// the contract went unanswered — a fact a review must be able to shout
    /// about, and one a bare empty list cannot carry.
    pub claims_at: Option<String>,
    /// Why the claims block this attempt wrote could not be read (§gh#235).
    ///
    /// Set only by the harvest off a finished attempt, and cleared the moment
    /// a good set arrives — an agent that submits properly has superseded its
    /// own bad block, and leaving the refusal beside the claims that replaced
    /// it would be the same mistake appending claims would be.
    ///
    /// `None` with no claims is the ordinary claimless attempt and stays quiet.
    /// `Some` is the one state that has to be loud: the agent did say what it
    /// did, and the board could not read it.
    pub claims_error: Option<String>,
    /// The auto-pick rule that released this attempt (gh#490), and the human
    /// who owns that rule. Both `None` on every attempt a person or an
    /// driving agent dispatched — which is what makes the provenance
    /// legible: an automation's work always says whose automation it is.
    pub automation: Option<String>,
    pub automation_owner: Option<String>,
}

impl Attempt {
    /// Who released this attempt, as recorded at dispatch.
    pub fn dispatcher(&self) -> Dispatcher {
        Dispatcher::agent(self.dispatched_by.clone(), self.dispatched_by_pane.clone())
    }
}

/// Who released a task.
///
/// The board dispatches *into* fresh panes, but the pane that does the
/// dispatching is usually not one of them: the common topology is one
/// long-lived driving chat the operator started and keeps around, which
/// releases many children. That pane is a session, not an attempt — so asking
/// "does a live attempt own this pane" answers `None` for exactly the case
/// provenance most needs to see, and every dispatch gets recorded as the
/// operator's.
///
/// So an agent is identified by its **pane**, which every command carries in
/// `HERDR_PANE_ID` whether or not the board started it, and named by its
/// **task** as well when the board did dispatch it — a board-dispatched chain
/// keeps the richer `via LIN-138` label rather than dropping to a pane id.
///
/// `Operator` is the narrow case it sounds like: a keypress on the board, or a
/// CLI run from a pane with no agent in it. Not merely "not an attempt".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dispatcher {
    Operator,
    Agent {
        /// The agent's own task, when the board dispatched it. Absent for an
        /// driving chat, which is the usual case.
        task: Option<String>,
        /// The pane it ran from — a delivery address, and the only identifier
        /// that is always available.
        pane: Option<String>,
    },
}

impl Dispatcher {
    /// An agent known by a task, a pane, or both. Knowing neither is the
    /// operator, so that case cannot be constructed by accident.
    pub fn agent(task: Option<String>, pane: Option<String>) -> Dispatcher {
        match (task, pane) {
            (None, None) => Dispatcher::Operator,
            (task, pane) => Dispatcher::Agent { task, pane },
        }
    }

    pub fn is_agent(&self) -> bool {
        matches!(self, Dispatcher::Agent { .. })
    }

    /// The parent's task id, when the board dispatched the parent too.
    pub fn task(&self) -> Option<&str> {
        match self {
            Dispatcher::Agent { task, .. } => task.as_deref(),
            Dispatcher::Operator => None,
        }
    }

    /// The pane the dispatch came from — where to reach the dispatcher.
    pub fn pane(&self) -> Option<&str> {
        match self {
            Dispatcher::Agent { pane, .. } => pane.as_deref(),
            Dispatcher::Operator => None,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// An attempt row with nothing on it, for the tests that care about two
    /// fields out of forty. Shared across the crate because the alternative is
    /// a forty-line literal in every module that needs one, and forty-line
    /// literals are what makes adding a column a day's work.
    pub(crate) fn blank_attempt() -> Attempt {
        Attempt {
            automation: None,
            automation_owner: None,
            id: 1,
            task_id: "gh:o/r#1".into(),
            pane_id: Some("chat-1".into()),
            workspace: "offhand".into(),
            runtime: "claude-code".into(),
            worktree: None,
            board_managed: true,
            repo_path: None,
            branch: Some("board/gh-1".into()),
            started_at: "2026-08-09T09:00:00Z".into(),
            ended_at: None,
            outcome: None,
            account: None,
            missing_ticks: 0,
            agent_status: None,
            dispatched_by: None,
            dispatched_by_pane: None,
            base_sha: None,
            stacked_on: None,
            saw_working: true,
            settled_at: None,
            reopened: 0,
            resumes: 0,
            screen_print: None,
            screen_at: None,
            nudges: 0,
            nudged_at: None,
            blocked_count: 0,
            overrun_warned_at: None,
            collectable_at: None,
            collected_at: None,
            cache_sweepable_at: None,
            cache_swept_at: None,
            dispatched_by_device: None,
            dispatched_by_user: None,
            dispatched_by_verified: false,
            billed_to: None,
            chat_archivable_at: None,
            chat_archived_at: None,
            tokens: None,
            token_models: None,
            token_agents: None,
            context: None,
            model: None,
            claims: Vec::new(),
            claims_at: None,
            claims_error: None,
        }
    }

    fn d() -> Derivation {
        Derivation::default()
    }

    #[test]
    fn a_task_id_says_which_repo_a_github_number_belongs_to() {
        assert_eq!(
            gh_repo("gh:Florin-AS/tripletex-mcp#2"),
            Some("Florin-AS/tripletex-mcp")
        );
        // The pull-request form of the id, which carries `!` instead.
        assert_eq!(
            gh_repo("gh:bredebjorhovd/OIOS!10"),
            Some("bredebjorhovd/OIOS")
        );
        assert_eq!(gh_repo_name("gh:bredebjorhovd/OIOS!10"), Some("OIOS"));
        // A legacy Linear id (gh#471) names no repo, and its identifier needs
        // none: `LIN-142` was unique across every project the board watched.
        assert_eq!(gh_repo("linear:LIN-142"), None);
        assert_eq!(gh_repo_name("linear:LIN-142"), None);
    }

    #[test]
    fn a_pull_request_url_says_its_number() {
        assert_eq!(
            pr_url_number("https://github.com/Florin-AS/tally/pull/507"),
            Some(507)
        );
        // The tabs of the same page still name the same pull request.
        assert_eq!(
            pr_url_number("https://github.com/Florin-AS/tally/pull/507/files"),
            Some(507)
        );
        // An issue URL is not a pull request, and inventing a number off one
        // would put the wrong address in a merge confirmation.
        assert_eq!(
            pr_url_number("https://github.com/Florin-AS/tally/issues/507"),
            None
        );
        assert_eq!(pr_url_number("https://example.com/pull/507"), None);
    }

    #[test]
    fn no_attempt_unstarted_is_ready() {
        assert_eq!(derive_state(d()), BoardState::Ready);
    }

    #[test]
    fn live_attempt_outranks_upstream_ready() {
        // The spec's explicit rule: an open live attempt always outranks
        // upstream `ready`.
        let s = derive_state(Derivation {
            upstream: UpstreamState::Unstarted,
            live: Some(AgentStatus::Working),
            ..d()
        });
        assert_eq!(s, BoardState::Working);
    }

    #[test]
    fn blocked_agent_is_blocked() {
        assert_eq!(
            derive_state(Derivation {
                live: Some(AgentStatus::Blocked),
                ..d()
            }),
            BoardState::Blocked
        );
    }

    #[test]
    fn idle_agent_without_pr_stays_working() {
        // "only finalize on explicit done detection or user action" — an idle
        // agent may just be between turns.
        for st in [AgentStatus::Idle, AgentStatus::Done, AgentStatus::Unknown] {
            assert_eq!(
                derive_state(Derivation {
                    live: Some(st),
                    open_pr: false,
                    ..d()
                }),
                BoardState::Working,
                "{st:?} without a PR must stay working"
            );
        }
    }

    #[test]
    fn idle_agent_with_pr_is_review() {
        assert_eq!(
            derive_state(Derivation {
                live: Some(AgentStatus::Done),
                open_pr: true,
                ..d()
            }),
            BoardState::Review
        );
    }

    #[test]
    fn missing_pane_is_failed() {
        assert_eq!(
            derive_state(Derivation {
                live: Some(AgentStatus::Missing),
                ..d()
            }),
            BoardState::Failed
        );
    }

    /// gh#289. A layer whose parent was asked to change is not reviewable in
    /// good faith: the ordered upstack rebase will move these commits onto a
    /// different base, so the diff a human reads now is not the diff that lands
    /// — and the bad outcome is not wasted reading, it is an approval that
    /// outlives the diff it was given to.
    #[test]
    fn a_layer_whose_parent_was_asked_to_change_comes_out_of_review() {
        for live in [None, Some(AgentStatus::Idle), Some(AgentStatus::Done)] {
            assert_eq!(
                derive_state(Derivation {
                    live,
                    open_pr: true,
                    changes_below: true,
                    ..d()
                }),
                BoardState::Blocked,
                "{live:?}",
            );
            // …and without the parent's objection it is the `review` row it
            // always was.
            assert_eq!(
                derive_state(Derivation {
                    live,
                    open_pr: true,
                    ..d()
                }),
                BoardState::Review,
                "{live:?}",
            );
        }
    }

    /// The other half of gh#289's answer: an agent still working is *informed*,
    /// not stopped. Its run may produce work that survives the ordered rebase,
    /// and stopping it to save a rebase spends a context to save a `git rebase`.
    #[test]
    fn a_working_layer_is_not_stopped_by_a_parent_asked_to_change() {
        assert_eq!(
            derive_state(Derivation {
                live: Some(AgentStatus::Working),
                open_pr: true,
                changes_below: true,
                ..d()
            }),
            BoardState::Working
        );
        // Nor is a row with no pull request of its own to be unreviewable about.
        assert_eq!(
            derive_state(Derivation {
                live: Some(AgentStatus::Idle),
                open_pr: false,
                changes_below: true,
                ..d()
            }),
            BoardState::Working
        );
    }

    #[test]
    fn terminal_upstream_is_done_even_with_live_attempt() {
        assert_eq!(
            derive_state(Derivation {
                upstream: UpstreamState::Terminal,
                live: Some(AgentStatus::Working),
                ..d()
            }),
            BoardState::Done
        );
    }

    /// A reaped task keeps its attempts, so it must not derive back into a state
    /// that invites a retry: there is no issue left to retry against. `gone` is
    /// terminal exactly like `terminal`, which is also what lets `gc` collect its
    /// checkout instead of stranding it.
    #[test]
    fn a_task_gone_from_upstream_is_done_whatever_its_last_attempt_did() {
        for outcome in [
            None,
            Some(Outcome::Cancelled),
            Some(Outcome::Failed),
            Some(Outcome::Done),
        ] {
            let s = derive_state(Derivation {
                upstream: UpstreamState::Gone,
                last_outcome: outcome,
                ..d()
            });
            assert_eq!(s, BoardState::Done, "gone + {outcome:?}");
        }
        assert!(BoardState::Done.is_terminal(), "so gc can collect it");
    }

    #[test]
    fn gone_and_terminal_are_both_final_upstream() {
        assert!(UpstreamState::Gone.is_final());
        assert!(UpstreamState::Terminal.is_final());
        assert!(!UpstreamState::Started.is_final());
        assert!(!UpstreamState::Unstarted.is_final());
    }

    #[test]
    fn upstream_states_round_trip() {
        for s in [
            UpstreamState::Unstarted,
            UpstreamState::Started,
            UpstreamState::Terminal,
            UpstreamState::Gone,
        ] {
            assert_eq!(UpstreamState::parse(s.as_str()), Some(s));
        }
    }

    /// Design-spec gap #5, resolved here: cancelling ends the attempt, not the
    /// issue. The row returns to `ready` with its history intact.
    #[test]
    fn cancelled_attempt_returns_to_ready() {
        assert_eq!(
            derive_state(Derivation {
                upstream: UpstreamState::Started,
                live: None,
                last_outcome: Some(Outcome::Cancelled),
                open_pr: false,
                changes_below: false,
                local_done: false,
            }),
            BoardState::Ready
        );
    }

    #[test]
    fn started_upstream_with_nothing_live_is_ready() {
        assert_eq!(
            derive_state(Derivation {
                upstream: UpstreamState::Started,
                ..d()
            }),
            BoardState::Ready
        );
    }

    #[test]
    fn failed_and_orphaned_attempts_are_failed() {
        for o in [Outcome::Failed, Outcome::Orphaned] {
            assert_eq!(
                derive_state(Derivation {
                    last_outcome: Some(o),
                    ..d()
                }),
                BoardState::Failed
            );
        }
    }

    #[test]
    fn done_attempt_is_review_not_done() {
        // herdr's `done` is our `review`; our `done` means the issue is closed.
        assert_eq!(
            derive_state(Derivation {
                last_outcome: Some(Outcome::Done),
                ..d()
            }),
            BoardState::Review
        );
    }

    #[test]
    fn open_pr_outranks_a_failed_attempt() {
        assert_eq!(
            derive_state(Derivation {
                last_outcome: Some(Outcome::Failed),
                open_pr: true,
                ..d()
            }),
            BoardState::Review
        );
    }

    #[test]
    fn local_done_outranks_everything() {
        // Otherwise `d mark done` undoes itself on the next poll.
        assert_eq!(
            derive_state(Derivation {
                upstream: UpstreamState::Started,
                live: Some(AgentStatus::Working),
                last_outcome: Some(Outcome::Failed),
                open_pr: true,
                changes_below: true,
                local_done: true,
            }),
            BoardState::Done
        );
    }

    #[test]
    fn blocked_holds_a_pane() {
        // It counts against max_concurrent_per_workspace.
        assert!(BoardState::Blocked.holds_pane());
        assert!(BoardState::Working.holds_pane());
        assert!(!BoardState::Ready.holds_pane());
        assert!(!BoardState::Review.holds_pane());
    }

    #[test]
    fn glyphs_are_shape_distinct() {
        // Rule 3: every state must survive color being stripped.
        let g: Vec<_> = BoardState::SECTION_ORDER
            .iter()
            .map(|s| s.glyph())
            .collect();
        let mut seen = g.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), g.len(), "two states share a glyph: {g:?}");
    }

}
