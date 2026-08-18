//! Board vocabulary shared by every surface: the state enum with its section
//! order and glyphs, the `TaskRow` wire shape `WatchBoard` streams, and the
//! view derivations (sections, the `f`/`/` filter cycle, what each row says)
//! that both viewports draw from.
//!
//! This lives in proto, not in `comet-board`, for the same reason the rest of
//! [`crate::view`] does: the viewports (the gpui app's board panel, and the
//! iOS board screen, which mirrors these derivations against the same
//! `TaskRow` wire shape) render board rows without depending on the board
//! crate, and a glyph, section order or filter position that differs between
//! surfaces is a real bug.
//! `comet-board` re-exports `BoardState` and `TaskRow`, so board-side code and
//! its tests are unchanged.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Board-level task state. Note the deliberate divergence from herdr's
/// vocabulary: herdr's `done` means "agent finished, you haven't looked", which
/// is our `review`. Our `done` means the issue is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BoardState {
    Blocked,
    Working,
    Ready,
    Review,
    Failed,
    Done,
}

impl BoardState {
    /// Fixed section order on the board: blocked → working → ready → review →
    /// failed → done.
    pub const SECTION_ORDER: [BoardState; 6] = [
        BoardState::Blocked,
        BoardState::Working,
        BoardState::Ready,
        BoardState::Review,
        BoardState::Failed,
        BoardState::Done,
    ];

    /// Shape-distinct glyph per state. Three shape families on purpose —
    /// pointed (`▲ ▸`), round (`● ·`), crossed (`✓ ✕`) — so every state
    /// survives color being stripped.
    pub fn glyph(self) -> &'static str {
        match self {
            BoardState::Blocked => "▲",
            BoardState::Working => "●",
            BoardState::Ready => "▸",
            BoardState::Review => "✓",
            BoardState::Failed => "✕",
            BoardState::Done => "·",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            BoardState::Blocked => "BLOCKED",
            BoardState::Working => "WORKING",
            BoardState::Ready => "READY",
            BoardState::Review => "REVIEW",
            BoardState::Failed => "FAILED",
            BoardState::Done => "DONE",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            BoardState::Blocked => "blocked",
            BoardState::Working => "working",
            BoardState::Ready => "ready",
            BoardState::Review => "review",
            BoardState::Failed => "failed",
            BoardState::Done => "done",
        }
    }

    pub fn parse(s: &str) -> Option<BoardState> {
        Some(match s {
            "blocked" => BoardState::Blocked,
            "working" => BoardState::Working,
            "ready" => BoardState::Ready,
            "review" => BoardState::Review,
            "failed" => BoardState::Failed,
            "done" => BoardState::Done,
            _ => return None,
        })
    }

    /// A task holding a pane. `blocked` counts — it still occupies a pane, so it
    /// counts against `max_concurrent_per_workspace`. (Board policy, kept on
    /// the enum so the move here changed no call sites.)
    pub fn holds_pane(self) -> bool {
        matches!(self, BoardState::Working | BoardState::Blocked)
    }

    /// Finished for good, with no retry left to come.
    ///
    /// Only `done` qualifies: `review` is waiting for you and `failed` is
    /// waiting for a retry, and both of those still have a use for the attempt's
    /// worktree — a retry reuses the checkout already holding the branch. This
    /// is the filter `gc` prunes by.
    pub fn is_terminal(self) -> bool {
        matches!(self, BoardState::Done)
    }
}

impl fmt::Display for BoardState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One runtime a dispatch can be pointed at, as `ListBoardRuntimes` reports it.
///
/// `name` is exactly what `routing.toml` and the `DispatchTask` override
/// accept; `label` is the human spelling a picker shows; `harness` is what the
/// board resolves the name to, which is how an account picker knows which saved
/// logins a runtime could spend (gh#74).
///
/// Here rather than in `comet-board` for the same reason [`TaskRow`] is: both
/// viewports deserialize it, and neither depends on the board crate.
/// `comet_board::runtime` re-exports it and owns what the list contains.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeOption {
    pub name: String,
    pub label: String,
    pub harness: crate::HarnessId,
    /// Why this runtime could **not** start on the device the list answers for
    /// (gh#187), or `None` when it could.
    ///
    /// The whole reason `ListBoardRuntimes` is relay-forwardable: the catalog
    /// used to be a constant, so a picker offered OpenCode to a box that had
    /// never installed it, the dispatch was accepted, a worktree was cut and a
    /// chat created — and only the harness spawn discovered the missing CLI.
    /// The board had spent the expensive part before checking the cheap fact.
    ///
    /// Defaults to `None` so a box too old to answer reads as available, which
    /// is exactly what it used to promise. An option is still *listed* when it
    /// is unavailable: an operator who expects OpenCode on a box should learn
    /// that it is not installed, not find the row quietly absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<RuntimeUnavailable>,
}

impl RuntimeOption {
    /// Could a dispatch pointed at this runtime actually start?
    pub fn available(&self) -> bool {
        self.unavailable.is_none()
    }

    /// What a picker row says beside the label when it cannot start — `None`
    /// on a runtime that can.
    pub fn note(&self) -> Option<&'static str> {
        Some(self.unavailable?.reason())
    }
}

/// Why a runtime cannot start on the device a picker (or a dispatch) is asking
/// about (gh#187).
///
/// Two axes, deliberately named apart, because they are two different jobs for
/// whoever reads them: a missing CLI is an install, a signed-out one is a
/// login. Codex sat installed-but-signed-out on the box for twenty minutes,
/// and a dispatch in that window looked identical from the picker to one that
/// would have worked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeUnavailable {
    /// The CLI is nowhere this device looks for it.
    NotInstalled,
    /// The CLI is there and has no usable credential.
    SignedOut,
    /// This build has no adapter for the runtime at all — nothing an operator
    /// can install will change it, which is why it is not [`NotInstalled`].
    ///
    /// [`NotInstalled`]: RuntimeUnavailable::NotInstalled
    Unsupported,
}

impl RuntimeUnavailable {
    /// The short phrase a picker chip, a board row or `doctor` carries. One
    /// spelling per reason, for the same rule the billing words follow: a
    /// warning worded three ways is three warnings nobody recognises as one.
    pub fn reason(self) -> &'static str {
        match self {
            RuntimeUnavailable::NotInstalled => "not installed",
            RuntimeUnavailable::SignedOut => "signed out",
            RuntimeUnavailable::Unsupported => "no adapter in this build",
        }
    }

    /// What to do about it, when there is something to do.
    pub fn hint(self) -> Option<&'static str> {
        match self {
            RuntimeUnavailable::NotInstalled => Some("install its CLI"),
            RuntimeUnavailable::SignedOut => Some("sign it in"),
            // Nothing an operator does to the box will help.
            RuntimeUnavailable::Unsupported => None,
        }
    }

    /// The full sentence, naming the runtime and which of the two is wrong.
    ///
    /// Shared rather than composed per surface because four of them say it —
    /// the engine's refusal, the `comet-board` CLI's pre-flight, the desktop
    /// picker and the phone's — and an operator who learned the words in one
    /// place has to recognise them in the next. `runtime` is the canonical
    /// *name* rather than the picker label, because that is the spelling a
    /// route or a `--runtime` flag has to be fixed in.
    ///
    /// "On the host" and not "here": every surface that says this may be
    /// asking about another device, and three of them usually are.
    pub fn refusal(self, runtime: &str) -> String {
        match self {
            RuntimeUnavailable::Unsupported => {
                format!("runtime `{runtime}` has no adapter in this build")
            }
            other => format!(
                "runtime `{runtime}` is {} on the host — {}",
                other.reason(),
                other.hint().unwrap_or_default()
            ),
        }
    }
}

/// The mark a pinned orchestrator's session row carries, on both viewports.
///
/// Shape-distinct from every [`BoardState::glyph`] and from the session dot, on
/// the same rule those follow: it has to survive colour being stripped, because
/// one row in the list meaning something different from all the others is the
/// whole point of drawing it.
pub const ORCHESTRATOR_GLYPH: &str = "◆";

/// Which chat, if any, is pinned as this board's orchestrator (gh#104).
///
/// A frame of its own rather than a field on [`TaskRow`], and a *stream* rather
/// than a read, for the same reason: the pin is a property of the board and not
/// of any task on it, and every surface that renders it — the sidebar, on both
/// viewports — needs it before a board panel has ever been opened. Reading it
/// off `ReadBoardConfig` would work and would cost a git probe per space every
/// time, which is the wrong price for a glyph.
///
/// A struct rather than a bare `Option<String>` because this is the frame a
/// pinned-chat feature grows in: `null` today means unpinned, and a field added
/// beside it later does not change what an old client already parses.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrchestratorPin {
    /// The pinned chat's id. `None` = this board has no orchestrator, which is
    /// the default and a legitimate way to run one.
    #[serde(default)]
    pub chat_id: Option<String>,
}

/// One layer of a stacked pull request, as a sibling row sees it (gh#283).
///
/// Carried on every member's row rather than left for a reader to join for
/// itself, because the two questions a stack raises are both about the *other*
/// layers: which one is this, and can the ones underneath merge. A surface with
/// the whole board in hand could join by [`RowStack::number`]; the `list --json`
/// reader the agent conventions teach is looking at one row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackLayer {
    /// The task id of the row that carries this layer — where a surface
    /// navigates to when somebody picks it out of the map.
    pub id: String,
    /// What that row prints as, for a surface with no room for a map.
    pub identifier: String,
    pub pr_number: Option<i64>,
    /// GitHub's place for it in the stack, counting from the bottom. `None`
    /// where the preview did not say; [`RowStack::layers`] is ordered
    /// regardless, so the order is not read off this.
    pub position: Option<i64>,
    /// The pull request is still open. A layer that has landed is no longer in
    /// anybody's way, which is what keeps a merged parent out of
    /// [`landing`]'s answer.
    pub open: bool,
    /// That layer's own `mergeable_state`, so the AND that answers "can this
    /// land" can be made without a second lookup — and so a map can mark the
    /// layer that is stuck.
    pub mergeable: Option<String>,
    /// A reviewer asked this layer to change, and nothing has said otherwise
    /// since (gh#289).
    ///
    /// The other fact about a layer that is a fact about every layer above it:
    /// when this one pushes its fix, its direct child must launch one ordered
    /// upstack rebase that carries them all forward. Carried beside
    /// [`mergeable`](Self::mergeable) because it is read the same way — ANDed
    /// down the chain by [`landing`] — and so a map can mark the layer the stack
    /// is waiting on rather than only counting it.
    #[serde(default)]
    pub changes_requested: bool,
}

/// A row's place in a stacked pull request, and the map of its siblings
/// (gh#283).
///
/// GitHub's stacks are an ordered chain — each layer targets the branch below
/// it, the bottom targets trunk, and merging a layer merges everything below it
/// with it, atomically. That last half is why the row carries the whole map
/// rather than only its own position: what merging *this* pull request does
/// depends entirely on layers that are other rows on the board.
///
/// The map is what the board can see. A stack whose lower layers are pull
/// requests in a repository the board does not poll has fewer [`layers`] than
/// [`size`] — so the count stays GitHub's, never `layers.len()`, and a reader
/// is never told a stack is shorter than it is.
///
/// [`layers`]: RowStack::layers
/// [`size`]: RowStack::size
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowStack {
    /// Identifies the stack. Unique per repository, like a pull request number
    /// — two repos can both have a stack 3, and the board scopes the grouping
    /// accordingly.
    pub number: i64,
    /// This row's place in it, counting from the bottom.
    pub position: Option<i64>,
    /// How many layers GitHub says the stack holds.
    pub size: Option<i64>,
    /// The branch the *stack* lands on — the bottom layer's base, and the only
    /// branch in a stack that is not itself somebody's pull request.
    pub base_ref: Option<String>,
    /// Every layer the board can see, bottom first, this row included.
    pub layers: Vec<StackLayer>,
}

impl RowStack {
    /// The layers under `id`, bottom first — what merging that row would take
    /// with it.
    ///
    /// Read off the order rather than off [`StackLayer::position`], which the
    /// preview leaves out often enough that a rule keyed on it would go quiet
    /// exactly when the board is least sure of the topology.
    pub fn below(&self, id: &str) -> &[StackLayer] {
        match self.layers.iter().position(|layer| layer.id == id) {
            Some(i) => &self.layers[..i],
            None => &[],
        }
    }

    /// The layers on top of `id`, nearest first — the ones a change to that row
    /// moves the ground under (gh#289).
    ///
    /// [`below`](Self::below)'s mirror, and the direction propagation runs in:
    /// merging looks down, feedback fans up. Nearest first rather than bottom
    /// first, because the nearest layer is the one whose base moves.
    pub fn above(&self, id: &str) -> &[StackLayer] {
        match self.layers.iter().position(|layer| layer.id == id) {
            Some(i) => &self.layers[i + 1..],
            None => &[],
        }
    }

    /// One layer by task id.
    pub fn layer(&self, id: &str) -> Option<&StackLayer> {
        self.layers.iter().find(|layer| layer.id == id)
    }
}

/// One task, in the shape callers are promised: herdr-board's `list --json`
/// contract with the pane→chat rename applied.
///
/// This shape is a published contract, consumed three ways: `WatchBoard`
/// streams it, the `comet-board` CLI prints it (§board-cli, verbatim), and the
/// agent conventions text teaches orchestrating agents to poll it. Field
/// renames from herdr-board are exactly the two the port dictates — `pane_id`
/// → `chat_id`, `dispatched_by_pane` → `dispatched_by_chat` — because the
/// values *are* chat ids now, and a contract that lies about what its ids
/// address is worse than one that renames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRow {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub state: String,
    pub source: String,
    pub url: String,
    pub labels: Vec<String>,
    /// False when no route matches, or when the issue is gone upstream: the
    /// task is on the board but cannot be dispatched, and `dispatch` refuses it.
    pub dispatchable: bool,
    /// The issue behind this row no longer exists upstream. The row is kept for
    /// the attempts on it; there is nothing left to work on.
    pub gone: bool,
    pub route: Option<String>,
    /// The comet space (herdr's workspace — the config key keeps that name).
    pub workspace: Option<String>,
    pub runtime: Option<String>,
    /// The live attempt's chat (herdr-board's `pane_id`).
    pub chat_id: Option<String>,
    /// The chat that authored the latest reviewable attempt, including after
    /// that attempt has settled. Kept separate from `chat_id`: a finished chat
    /// is review context, not a live agent consuming a concurrency slot.
    #[serde(default)]
    pub review_chat_id: Option<String>,
    /// Set on `review` rows, which is how a PR reaches an orchestrator.
    pub pr_url: Option<String>,
    pub pr_number: Option<i64>,
    /// The branch the pull request merges *into* (gh#282). Trunk for a
    /// standalone one; mid-stack it is the branch of the layer below, which is
    /// what makes [`pr_mergeable`](Self::pr_mergeable) mean less than it looks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_base_ref: Option<String>,
    /// GitHub's `mergeable_state` for this pull request **alone**: `clean`,
    /// `behind`, `dirty`, `blocked`, `unstable`, `draft`. `None` where it has
    /// not been asked — it costs a call per open PR and rides the full sweep.
    ///
    /// Never render this unqualified. For a layer of a stack, `clean` means
    /// clean *against the layer below*, not "ready to land", and the board
    /// spent this whole issue not saying the second when GitHub said the first
    /// (gh#283). [`landing`] is the answer to the question a reader is actually
    /// asking; this is the raw fact it is derived from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_mergeable: Option<String>,
    /// A layer below this one has been asked to change (gh#289) — the pull
    /// request number of the nearest such layer, or absent when none has.
    ///
    /// The one fact about a stack that is not about merging: this pull request
    /// may be perfectly clean and still be unreviewable, because the changed
    /// layer's direct child must replay this path onto rewritten history. A
    /// human who reads the diff now is reading a diff that is going to move,
    /// and the worse outcome is that they approve it. So [`landing`] answers
    /// with it before it answers anything else, and the row leaves the review
    /// section while it stands.
    ///
    /// Derived board-side rather than read off [`stack`](Self::stack), because
    /// the dependency edge is wider than GitHub's own stack object: a layer the
    /// board dispatched onto a sibling's branch (gh#285) is stacked on it
    /// whether or not GitHub was told so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes_below: Option<i64>,
    /// Whether this pull request can land, with the layers below it taken into
    /// account: `ready`, `waiting-on-stack`, `not-clean`, `changes-below` — or
    /// absent, meaning nobody has asked GitHub yet (gh#283).
    ///
    /// The verdict rather than its parts, because the AND across a stack is the
    /// one thing every reader of this contract would otherwise have to
    /// reimplement, and a reader who skipped it would believe `clean`. Derived
    /// by [`landing`] from the two fields above and [`stack`](Self::stack); the
    /// board fills it with that same function, so the wire and the viewports
    /// can never come to disagree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landing: Option<String>,
    /// Set when GitHub says this pull request is one layer of a stack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<RowStack>,
    pub branch: Option<String>,
    /// Parent task id when the board dispatched the releasing agent too. Null
    /// on its own does **not** mean the operator: an orchestrating chat has no
    /// attempt and so no task id — read `dispatched_by_chat` as well.
    pub dispatched_by: Option<String>,
    /// The chat the dispatch ran from, when an agent was in it (herdr-board's
    /// `dispatched_by_pane`). Set for every agent-released row, and the address
    /// a parent can be reached at. Both this and `dispatched_by` null means the
    /// operator released it.
    pub dispatched_by_chat: Option<String>,
    /// How the most recent *ended* attempt ended: `done`, `failed`, `cancelled`
    /// or `orphaned`. Null means no attempt has ever ended.
    ///
    /// This is what makes cancellation legible to a parent agent. `cancelled`
    /// derives back to `ready` — deliberately, since the issue is still owed —
    /// so without this field a child the operator killed is indistinguishable
    /// from one that was never dispatched. Set even while a *newer* attempt is
    /// live, so a retry does not erase how the previous one went.
    pub last_outcome: Option<String>,
    /// When that attempt ended (RFC 3339). Pairs with `last_outcome` so a
    /// long-lived poller can tell a fresh cancellation from one it already saw.
    pub last_outcome_at: Option<String>,
    pub attempts: usize,
    /// How many times the board closed this row's attempt and then found its
    /// agent still working (herdr-board gh#34's honesty field).
    pub reopened: i64,
    /// When the issue last changed upstream (RFC 3339). Missing on a row that
    /// predates this field; empty string and `None` mean the same thing.
    ///
    /// Fork addition to herdr-board's contract, which never needed it — its
    /// view read the internal task. The `done` section is bounded to today here,
    /// and a viewport that cannot tell today's close from history cannot draw
    /// that section, so the wire carries the timestamp instead of two viewports
    /// guessing at one.
    #[serde(default)]
    pub updated_at: String,
    /// When the live attempt started (RFC 3339), for the elapsed counter on a
    /// working or blocked row. `None` when nothing is running.
    #[serde(default)]
    pub started_at: Option<String>,
    /// The agent account whose subscription this row's attempt spends — the
    /// slot id the attempt recorded, falling back to the route's default for a
    /// row nothing has run on yet. `None` is the device's own CLI login, which
    /// is also every row on a single-account box (gh#59).
    #[serde(default)]
    pub account: Option<String>,
    /// The human who released this row's attempt — an email where the box knows
    /// one, else the user id (gh#74). Null where nobody said: a `comet-board`
    /// dispatch, an agent's, and every attempt from before the frontends sent
    /// it.
    ///
    /// How much the name is worth is
    /// [`dispatched_by_verified`](Self::dispatched_by_verified), and a surface
    /// that renders one without the other is asserting something the board did
    /// not. Never a credential either way: what a run may spend stays the
    /// explicit `account` (gh#59).
    #[serde(default)]
    pub dispatched_by_user: Option<String>,
    /// Did the edge verify [`dispatched_by_user`](Self::dispatched_by_user)?
    /// (gh#161.)
    ///
    /// True: the relay stamped the caller's verified identity onto the frame
    /// and the box resolved it — a teammate's dispatch from their own laptop,
    /// and what `require-own` refuses on. False: a claim — a dispatch issued on
    /// the box itself (where nothing but the box can reach the IPC port), a
    /// verified caller nobody could name, and every attempt from before this
    /// existed. Defaults to false, so a row from an older box reads as the
    /// claim it is.
    #[serde(default)]
    pub dispatched_by_verified: bool,
    /// Whose subscription this row's attempt actually spends, as an email
    /// (gh#101): the [`account`](Self::account) slot's login, or the box's own
    /// CLI login when the dispatch named no slot.
    ///
    /// Resolved once, at dispatch, and recorded on the attempt — the slot id in
    /// `account` means nothing to a reader who has not saved that login, and
    /// the box's own login can change under a run that is still going. `None`
    /// on a row nothing has run on yet, and on attempts from before this
    /// existed.
    ///
    /// Compared against [`dispatched_by_user`](Self::dispatched_by_user), this
    /// is what makes a cross-billed run visible for its whole life — see
    /// [`cross_billed`].
    #[serde(default)]
    pub billed_to: Option<String>,
    /// The wall-clock cap one attempt on this row gets (gh#70's `max_duration`,
    /// in seconds), resolved route-then-defaults. `None` is uncapped.
    ///
    /// On the wire because the *elapsed* counter is worth nothing without it:
    /// "1h50m" says one thing under a two-hour cap and another under six, and
    /// only the board knows which — the routing config is the host's, and a
    /// viewport reading a relayed board has never seen it (gh#103).
    #[serde(default)]
    pub max_duration_secs: Option<u64>,
    /// How full this row's attempt has filled its agent's context window
    /// (gh#271) — the last level its harness reported.
    ///
    /// On the wire beside the elapsed clock and for the same reason: it is a
    /// *live* fact with a horizon. An attempt at 94% of its window is minutes
    /// from having the context it is working from compacted away, and that is
    /// a different situation from the same attempt at 20% — while the elapsed
    /// counter, the only other thing a watcher has, reads identically for
    /// both.
    ///
    /// `None` is "nothing reported" and must render as a blank, never as 0%: a
    /// harness that meters no window (opencode), a Claude CLI too old to
    /// answer, and every row from before this existed all land here, and an
    /// empty context is a very different claim from a silent one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<crate::ContextUsage>,
}

// ---------------------------------------------------------------------------
// View derivations
// ---------------------------------------------------------------------------
//
// Everything both viewports need to say the same thing about the same board,
// ported from herdr-board's `ui/state.rs` and `ui/render.rs`. The TUI renders
// today; the gpui app grows the same view later. A row's state, its section,
// its metadata and the filter that hides it are all derived here, once.

use chrono::{DateTime, Utc};

/// What the route column renders for a row nothing routes — and so what the
/// header says, and what a typed query has to match.
pub const NO_ROUTE: &str = "no route";

impl TaskRow {
    /// The board state this row carries. An unrecognized wire value defaults to
    /// `ready` — a schema-skewed row must read as waiting, not crash the view.
    pub fn state(&self) -> BoardState {
        BoardState::parse(&self.state).unwrap_or(BoardState::Ready)
    }

    /// The leading token a board row shows: the CLI's repo-qualified form,
    /// humanized — `tally #507`, not `gh#507` (gh#125).
    ///
    /// GitHub numbers issues per repository, so `gh#507` and `gh#44` can be
    /// different repos distinguishable only by a muted sub-line. The repo name
    /// is the half of the id that makes the identifier readable on its own; the
    /// owner stays out for the same reason it stays out of branch names. A
    /// Linear identifier (`LIN-142`) is already unique across the board and is
    /// shown unchanged, as is any id this rule cannot parse.
    ///
    /// The separator is the id's own, `#` or `!` — this qualifies a name, it
    /// does not replace one (gh#357). A `gh!508` row is a pull request nobody
    /// filed a ticket for (gh#344), and `gh!508` *is* its identifier; rendering
    /// it `tally #508` gives it a second name, and one already taken by
    /// whatever issue #508 is.
    pub fn display_identifier(&self) -> String {
        let tail = self
            .id
            .rfind(['#', '!'])
            .map(|ix| self.id.split_at(ix))
            .map(|(_, tail)| (&tail[..1], &tail[1..]))
            .filter(|(_, number)| !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()));
        match (gh_repo_name(&self.id), tail) {
            (Some(repo), Some((sep, number))) if !repo.is_empty() => {
                format!("{repo} {sep}{number}")
            }
            _ => self.identifier.clone(),
        }
    }

    /// Whether this row's name *is* its pull request: a `gh!508` row, a pull
    /// request the board never dispatched and no ticket ever asked for (gh#344).
    ///
    /// gh#357 does not except such a row — `gh!508` is that task's identifier,
    /// and the identifier is the name. What it changes is that naming the row
    /// and saying where it lives are the same act here, so a surface that would
    /// otherwise say both says one: `merge gh!508 into main`, not `merge gh!508
    /// (PR #508)`, and a review row that ends at `waiting on you`.
    pub fn is_pull_request(&self) -> bool {
        match (self.id.rsplit_once('!'), self.pr_number) {
            (Some((_, tail)), Some(number)) => tail == number.to_string(),
            _ => false,
        }
    }

    /// The short slug of this row's title (gh#364), for a surface that shows
    /// the identifier and has no room for the title itself.
    ///
    /// Not for the board pane, which draws the whole title in a column of its
    /// own — a slug there would be the title said twice and worse the second
    /// time. This is for the places that name a task in a token: the Active
    /// list, the Needs-you inbox, a branch.
    pub fn slug(&self) -> Option<String> {
        crate::view::slug::title_slug(&self.title)
    }
}

/// The `owner/repo` a GitHub task id names — `gh:Florin-AS/tally#507` →
/// `Florin-AS/tally`. `None` for a Linear id, which names no repo.
///
/// Lives here (and is re-exported by `comet_board::model`) because both the
/// board crate and every viewport read it: the board keys branches and panes on
/// it, the viewports key the row's leading token on it, and two parsers of one
/// id format is one too many.
pub fn gh_repo(task_id: &str) -> Option<&str> {
    // `!` is the pull-request form of the id: `gh:owner/repo!508`.
    task_id.strip_prefix("gh:")?.split(['#', '!']).next()
}

/// Just the repository's name — `Florin-AS/tally` → `tally`.
///
/// The owner is noise when you work with a handful of repos; the name is the
/// part you read, and so the part that names branches, panes and board rows.
pub fn gh_repo_name(task_id: &str) -> Option<&str> {
    gh_repo(task_id)?.rsplit('/').next()
}

/// Showing less than everything.
///
/// **A filter is a view, and changes nothing else.** Not what syncs, not what
/// is dispatchable, not what counts against `max_concurrent_per_workspace` — a
/// hidden row is still a live attempt with an agent working it. It lives here
/// so both viewports implement the `f`/`/` cycle the same way.
///
/// One filter at a time. `f` and `/` answer different questions — "show me one
/// project" and "where was that ticket" — and a board obeying both answers
/// neither.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Filter {
    #[default]
    All,
    /// One route, cycled with `f`.
    Route(String),
    /// The rows nothing routes, cycled with `f` after the named routes.
    ///
    /// Not a missing filter but a real one: a route that ANDs a repo with a
    /// label leaves most of that repo's backlog marked `no route`, on the board
    /// and deliberately undispatchable. Without a position of their own they
    /// are the rows you most need to see as a group and the only ones you
    /// cannot filter to — or away from.
    NoRoute,
    /// A substring of identifier, title or route, typed at `/`.
    Text(String),
}

impl Filter {
    pub fn active(&self) -> bool {
        !matches!(self, Filter::All)
    }

    pub fn matches(&self, row: &TaskRow) -> bool {
        match self {
            Filter::All => true,
            Filter::Route(r) => row.route.as_deref() == Some(r.as_str()),
            // The same field the named routes partition on, so every row is in
            // exactly one position of the cycle.
            Filter::NoRoute => row.route.is_none(),
            Filter::Text(q) => {
                let q = q.trim();
                q.is_empty()
                    || contains(&row.identifier, q)
                    // The row renders `tally #507`, so `/tally` must reach it —
                    // matching what is rendered is the least surprising rule.
                    || contains(&row.display_identifier(), q)
                    || contains(&row.title, q)
                    || row.route.as_deref().is_some_and(|r| contains(r, q))
                    // The route column renders `no route` on such a row, and
                    // matching what is rendered is the least surprising rule —
                    // it also makes `/no route` reach the group without having
                    // to know `f` exists at all.
                    || (row.route.is_none() && contains(NO_ROUTE, q))
            }
        }
    }

    /// What the header says is active. A filtered board that does not say so is
    /// a board that looks broken.
    pub fn label(&self) -> Option<String> {
        match self {
            Filter::All => None,
            Filter::Route(r) => Some(format!("filter: {r}")),
            // Said the way any other position is said, and in the words the
            // rows themselves use.
            Filter::NoRoute => Some(format!("filter: {NO_ROUTE}")),
            Filter::Text(q) => Some(format!("/{q}")),
        }
    }

    /// What to say when the filter has hidden every row, which otherwise draws
    /// a body with nothing in it at all.
    pub fn empty_note(&self) -> Option<String> {
        match self {
            Filter::Text(q) if q.trim().is_empty() => None,
            Filter::All => None,
            Filter::Route(r) => Some(format!(
                "Nothing on the board routes to {r}.  f moves on, F clears the filter."
            )),
            // Reachable when the last unrouted row leaves under the filter —
            // `f` never offers this position on a board where everything routes.
            Filter::NoRoute => Some(
                "Everything on the board has a route.  f moves on, F clears the filter."
                    .to_string(),
            ),
            Filter::Text(q) => Some(format!(
                "No identifier, title or route matches `{q}`.  F clears the filter."
            )),
        }
    }
}

fn contains(hay: &str, needle: &str) -> bool {
    hay.to_lowercase().contains(&needle.to_lowercase())
}

/// Every route with a row on the board, alphabetically.
///
/// Alphabetical rather than in board order: `f` is a tour of the projects, and
/// a tour whose order changes as rows move between sections is not one you can
/// learn. Only routes with a row that is *on* the board — offering a route
/// whose only rows are yesterday's `done` would filter to nothing.
pub fn routes_present(rows: &[TaskRow], now: DateTime<Utc>) -> Vec<String> {
    let mut out: Vec<String> = on_board(rows, now)
        .filter_map(|row| row.route.clone())
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// The rows `f` is a tour of: everything except history older than today.
///
/// Whether a position is offered and what it then shows have to be decided from
/// the same rows, or `f` offers stops that filter to nothing.
fn on_board(rows: &[TaskRow], now: DateTime<Utc>) -> impl Iterator<Item = &TaskRow> {
    rows.iter()
        .filter(move |row| row.state() != BoardState::Done || finished_today(row, now))
}

/// The positions `f` steps through, in order. `all` is the wrap, not a
/// position.
///
/// `no route` comes last, after the named routes: it is the odd one out, and
/// putting it at the end leaves the routes in a stable order as they come and
/// go rather than shifting every one of them by a place.
///
/// A position with no rows is never offered — an empty one is a keypress that
/// appears to do nothing, which reads as a bug. That is why a route whose only
/// row closed yesterday is left out, and it is the same reason `no route` is
/// skipped on a board where everything routes.
pub fn filter_cycle(rows: &[TaskRow], now: DateTime<Utc>) -> Vec<Filter> {
    let mut out: Vec<Filter> = routes_present(rows, now)
        .into_iter()
        .map(Filter::Route)
        .collect();
    if on_board(rows, now).any(|row| row.route.is_none()) {
        out.push(Filter::NoRoute);
    }
    out
}

/// Sections in fixed order, empty ones omitted entirely.
///
/// `done` is bounded to today. The header says "DONE today" and it means it:
/// every issue ever closed in a tracked repo derives to `done`, and ninety-odd
/// of them is a wall of history, not a board.
///
/// The filter is applied **here**, so everything downstream — the rows, the
/// header counts, which sections exist at all — agrees on what is shown
/// without each having to remember to ask. A section whose rows are all
/// filtered away is empty, and an empty section is already omitted.
pub fn sections<'a>(
    rows: &'a [TaskRow],
    filter: &Filter,
    now: DateTime<Utc>,
) -> Vec<(BoardState, Vec<&'a TaskRow>)> {
    BoardState::SECTION_ORDER
        .iter()
        .filter_map(|&state| {
            let section: Vec<&TaskRow> = rows
                .iter()
                .filter(|row| row.state() == state)
                .filter(|row| state != BoardState::Done || finished_today(row, now))
                .filter(|row| filter.matches(row))
                .collect();
            if section.is_empty() {
                None
            } else {
                Some((state, section))
            }
        })
        .collect()
}

/// One route's rows inside a section — the unit a hundred-row board is scanned
/// by (gh#125).
#[derive(Debug, PartialEq)]
pub struct SectionGroup<'a> {
    /// The route the group collects, keyed on the same field [`Filter::Route`]
    /// and the `f` cycle partition on. `None` is the `no route` group.
    pub route: Option<String>,
    pub rows: Vec<&'a TaskRow>,
}

impl SectionGroup<'_> {
    /// What the group header says: the route's name, or [`NO_ROUTE`] — the
    /// words the rows themselves use.
    pub fn label(&self) -> &str {
        self.route.as_deref().unwrap_or(NO_ROUTE)
    }
}

/// Whether a group starts folded, absent an operator's own toggle.
///
/// Unrouted rows are visibility-only by design — on the board, deliberately
/// undispatchable — so their group starts folded: worth a headline and a
/// count, never pole position over rows an `enter` can actually release. But
/// only on the unfiltered board: a filter is the operator asking for specific
/// rows, and a default fold that hides what was just asked for (`f` to the
/// `no route` position, `/` matching an unrouted title) fights the ask.
pub fn group_starts_collapsed(filter: &Filter, route: Option<&str>) -> bool {
    matches!(filter, Filter::All) && route.is_none()
}

/// [`sections`], with each section's rows grouped by route.
///
/// A flat section of a hundred rows cannot be scanned; the same rows as
/// "tally 34 · herdr-board 12 · …" can. Biggest group first — the summary reads
/// as a ranking — with ties alphabetical so equal groups do not trade places
/// between frames, and rows in board order within each group. `no route` sits
/// last regardless of size: it must never hold the top of a section, which is
/// the first selected row of the whole panel.
pub fn grouped_sections<'a>(
    rows: &'a [TaskRow],
    filter: &Filter,
    now: DateTime<Utc>,
) -> Vec<(BoardState, Vec<SectionGroup<'a>>)> {
    sections(rows, filter, now)
        .into_iter()
        .map(|(state, rows)| {
            let mut groups: Vec<SectionGroup<'a>> = Vec::new();
            for row in rows {
                match groups.iter_mut().find(|g| g.route == row.route) {
                    Some(group) => group.rows.push(row),
                    None => groups.push(SectionGroup {
                        route: row.route.clone(),
                        rows: vec![row],
                    }),
                }
            }
            groups.sort_by(|a, b| {
                a.route
                    .is_none()
                    .cmp(&b.route.is_none())
                    .then(b.rows.len().cmp(&a.rows.len()))
                    .then_with(|| a.route.cmp(&b.route))
            });
            (state, groups)
        })
        .collect()
}

/// Whether a section draws group headers at all.
///
/// One routed group needs none — three WORKING rows from one repo are readable
/// bare, and a header repeating what every row's leading token says is noise.
/// On the unfiltered board a lone `no route` group still draws its header: the
/// header is what keeps those rows folded, and folded rows need a headline to
/// be findable. Under a filter that lone header would only repeat what the
/// filter chip already says.
pub fn group_headers_shown(filter: &Filter, groups: &[SectionGroup]) -> bool {
    groups.len() > 1
        || (matches!(filter, Filter::All) && groups.first().is_some_and(|g| g.route.is_none()))
}

/// Was this task closed today, in the operator's own timezone?
///
/// Local midnight, not a rolling 24 hours: "today" is a thing a person means,
/// and a row dropping off at 14:37 because that is when it closed yesterday
/// would be baffling.
pub fn finished_today(row: &TaskRow, now: DateTime<Utc>) -> bool {
    let Ok(updated) = DateTime::parse_from_rfc3339(&row.updated_at) else {
        // An unparseable timestamp is not evidence of recency.
        return false;
    };
    let local = updated.with_timezone(&chrono::Local);
    local.date_naive() == now.with_timezone(&chrono::Local).date_naive()
}

/// `12s` / `9m04s` / `1h20m`. Minute resolution was rejected: a counter that
/// never visibly moves is not worth the redraw.
pub fn format_elapsed(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

/// A *cap* said the way a person configured it: `2h`, `45m`, `1h30m`.
///
/// Deliberately not [`format_elapsed`]: a cap is a round number somebody typed
/// into `routing.toml`, and rendering `defaults.max_duration = "2h"` as `2h00m`
/// makes the reader check whether the minutes mean anything. They never do.
pub fn format_cap(secs: u64) -> String {
    if secs < 60 {
        return format!("{secs}s");
    }
    let (h, m) = (secs / 3600, (secs % 3600) / 60);
    match (h, m) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h{m:02}m"),
    }
}

/// Coarser form for the header (`synced 12s`, `last synced 4m`).
pub fn format_age(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h", s / 3600)
    }
}

// ---- what rows say -------------------------------------------------------

/// Below this width, drop all metadata rather than wrap.
pub const NARROW_LIMIT: u16 = 60;

/// The width of the runtime and workspace cells in the terminal's grid.
///
/// A grid is a monospace idea: these two are the fields a column of rows reads
/// down, so the TUI pads them to a fixed cell. Nothing else does — see
/// [`MetaField`].
const RUNTIME_CELL: usize = 12;
const WS_CELL: usize = 11;

/// One fact in a row's metadata, and the terminal cell it occupies.
///
/// Two surfaces read the same facts and space them differently. The TUI pads
/// them into a monospace grid ([`row_metadata`]); the desktop joins them with
/// `·` into a right-aligned column set in a proportional font
/// ([`row_metadata_line`], gh#176), where a padding space is neither a column
/// nor invisible — it is just a gap of the wrong size. The *facts* are derived
/// once, here, so the two spacings can never come to disagree about them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MetaField {
    text: String,
    /// The cell this pads to in a monospace grid, or `None` for a field that
    /// simply follows the one before it.
    cell: Option<usize>,
}

impl MetaField {
    fn cell(text: impl Into<String>, width: usize) -> Self {
        Self {
            text: text.into(),
            cell: Some(width),
        }
    }

    fn flow(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            cell: None,
        }
    }
}

/// The right-hand metadata block for a row, already collapsed for `width`.
///
/// Below 60 columns everything goes rather than wrapping. Above it the block
/// is one line of fixed-width fields, so it cannot crowd the title.
///
/// herdr-board collapsed this block right-to-left (elapsed survives longest),
/// which only ever mattered because its `idle` marker widened the elapsed
/// field. comet's wire carries no agent-status "settled but quiet" state, so
/// elapsed stays short and the full block always fits at the widths that show
/// metadata at all — the collapse has nothing to collapse and is gone.
///
/// This is the *terminal's* spelling of the block. A proportional surface wants
/// [`row_metadata_line`] instead: same facts, no padding.
pub fn row_metadata(row: &TaskRow, selected: bool, width: u16, now: DateTime<Utc>) -> String {
    if width < NARROW_LIMIT {
        return String::new();
    }
    // Whose subscription this row is spending, when it is not the releaser's
    // (gh#101). Appended rather than folded into the per-state arms: it is true
    // of an attempt for its whole life — working, blocked, in review, long
    // closed — and a fact that survives the row changing section does not
    // belong inside the match on which section it is in.
    let base = state_metadata(row, selected, now);
    match billing_note(row) {
        Some(note) if base.trim().is_empty() => note,
        Some(note) => format!("{} · {note}", base.trim_end()),
        None => base,
    }
}

/// The facts each state is worth saying, in order, before any surface decides
/// how to space them.
fn state_metadata_fields(row: &TaskRow, selected: bool, now: DateTime<Utc>) -> Vec<MetaField> {
    match row.state() {
        BoardState::Working | BoardState::Blocked => {
            let elapsed = row
                .started_at
                .as_deref()
                .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
                // Never negative: a clock skew must not render as a count-up
                // from the future.
                .map(|start| format_elapsed((now - start.with_timezone(&Utc)).num_seconds().max(0)))
                .unwrap_or_default();
            vec![
                MetaField::cell(row.runtime.as_deref().unwrap_or(""), RUNTIME_CELL),
                MetaField::cell(ws(row), WS_CELL),
                MetaField::flow(elapsed),
                // After the clock, because it answers the question the clock
                // raises and cannot: two rows an hour in look identical, and
                // one of them is about to have its context compacted away
                // (gh#271). Quiet until there is something to say.
                MetaField::flow(context_note(row.context).unwrap_or_default()),
                // A layer waiting on the one below it (gh#289) is `blocked` with
                // no agent in it and no clock to run, so without this it is a row
                // in the loudest section of the board saying nothing at all about
                // why it is there. On a `working` layer it is the same fact and
                // still worth reading: the ground under this branch is moving.
                MetaField::flow(match row.changes_below {
                    Some(_) => landing_note(row).unwrap_or_default(),
                    None => String::new(),
                }),
            ]
        }
        BoardState::Failed => vec![MetaField::flow("pane exited without completing")],
        // What this row is, then what it needs, then where it lives — the
        // pull request last, and never first (gh#357). The row is already
        // named: its identifier leads the row on every viewport, and a bare
        // `PR #12` at the head of the block is a second number in the same
        // shape, competing to be read as the name of the work. It is a
        // location, so it goes after the facts and wears the preposition that
        // says so — which is also what keeps it from joining the list in
        // `waiting on PR #11 · PR #12`.
        BoardState::Review => match (row.pr_number, row.branch.as_deref()) {
            // A stacked PR says which layer it is, and — where the board has
            // asked — what merging it would actually do. The landing note
            // replaces "waiting on you" rather than joining it: both are the
            // row's call to action, and the one that names the branch says
            // strictly more (gh#283).
            (Some(n), _) => vec![
                MetaField::flow(stack_note(row).unwrap_or_default()),
                MetaField::flow(landing_note(row).unwrap_or_else(|| "waiting on you".into())),
                // Nothing to add on a row the pull request already names.
                MetaField::flow(match row.is_pull_request() {
                    true => String::new(),
                    false => format!("in PR #{n}"),
                }),
            ],
            // Finished on commits with no PR raised: say which branch, or the
            // row reads as "waiting on you" with nowhere to look.
            (None, Some(b)) => vec![MetaField::flow("no PR"), MetaField::flow(format!("on {b}"))],
            (None, None) => vec![MetaField::flow("waiting on you")],
        },
        BoardState::Ready => {
            // The route rides the group header and the repo the leading token
            // (gh#125), so the metadata keeps only what neither says: a routed
            // workspace whose name differs from the route's, and the cursor's
            // one affordance.
            let ws = row
                .workspace
                .as_deref()
                .filter(|w| row.route.as_deref() != Some(w))
                .unwrap_or_default();
            let note = if !row.dispatchable {
                // A property of the issue, not an affordance for the cursor —
                // so it shows on every such row, selected or not.
                NO_ROUTE
            } else if selected {
                "[enter to dispatch]"
            } else {
                ""
            };
            vec![MetaField::flow(ws), MetaField::flow(note)]
        }
        BoardState::Done => {
            // A row whose issue was deleted sits in `done` next to rows that
            // were properly closed, and the two are worth telling apart.
            let runtime = MetaField::cell(row.runtime.as_deref().unwrap_or(""), RUNTIME_CELL);
            if row.gone {
                vec![runtime, MetaField::flow("gone upstream")]
            } else {
                vec![runtime, MetaField::cell(ws(row), WS_CELL)]
            }
        }
    }
}

/// The fields padded into the terminal's grid: a celled field takes its whole
/// cell (empty or not, so the column below it stays a column), and a flowing
/// one either continues the cell before it or is separated by the board's `·`.
fn state_metadata(row: &TaskRow, selected: bool, now: DateTime<Utc>) -> String {
    let mut out = String::new();
    let mut after_cell = false;
    for field in state_metadata_fields(row, selected, now) {
        match field.cell {
            Some(width) => {
                out.push_str(&fixed(&field.text, width));
                after_cell = true;
            }
            None if field.text.is_empty() => {}
            None => {
                if !out.is_empty() && !after_cell {
                    out.push_str(" · ");
                }
                out.push_str(&field.text);
                after_cell = false;
            }
        }
    }
    out
}

/// The same facts as [`row_metadata`], as the discrete facts they are: no
/// padding, nothing empty, billing note last.
///
/// A surface that sets metadata in a proportional font wants these — the grid
/// [`row_metadata`] pads to only exists in a terminal, and pasted into a
/// desktop row it is a ragged gap rather than a column (gh#176).
pub fn row_metadata_fields(row: &TaskRow, selected: bool, now: DateTime<Utc>) -> Vec<String> {
    let mut out: Vec<String> = state_metadata_fields(row, selected, now)
        .into_iter()
        .map(|field| field.text)
        .filter(|text| !text.is_empty())
        .collect();
    out.extend(billing_note(row));
    out
}

/// [`row_metadata_fields`] as one line, joined the way the board joins facts
/// everywhere else.
pub fn row_metadata_line(row: &TaskRow, selected: bool, now: DateTime<Utc>) -> String {
    row_metadata_fields(row, selected, now).join(" · ")
}

// ---------------------------------------------------------------------------
// Stacks: what `mergeable` means when a pull request is a layer (gh#283)
// ---------------------------------------------------------------------------
//
// GitHub answers `mergeable_state` per pull request, against that pull
// request's own base. For a standalone PR the base is trunk and the answer is
// the whole story. For a layer of a stack the base is the layer below, so
// `clean` means "clean against the branch underneath me" — and the board reader
// asking whether to merge reads it as "ready to land", which it is not until
// everything below it is clean too.
//
// So the vocabulary is one step removed from GitHub's: [`Landing`] is what the
// board is willing to *claim*, and it is derived by ANDing this layer's answer
// with every open layer under it — GitHub's own semantics, where merging a
// layer merges or queues everything below it, atomically. A layer below that
// nobody has asked about yet takes the answer to "clean against its base" and
// no further. Not knowing is never rounded up to ready.

/// The one `mergeable_state` that is not an objection.
const MERGEABLE_CLEAN: &str = "clean";

/// Whether a pull request can land, said honestly for a layer of a stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Landing<'a> {
    /// This pull request merges, and so does every open layer under it —
    /// merging it lands all `below` of them with it. `below` is 0 for a
    /// standalone PR and for the bottom of a stack.
    Ready { below: usize },
    /// Clean against its own base and no further: mid-stack, that base is the
    /// layer below rather than trunk. `blocker` is the lowest layer under it
    /// that GitHub objects to, or `None` when the layers below simply have not
    /// been asked yet — "I do not know" and "I know it is stuck" are different
    /// answers and neither is "ready".
    CleanAgainstBase { blocker: Option<&'a StackLayer> },
    /// GitHub objects to this pull request itself, in its own vocabulary:
    /// `behind`, `dirty`, `blocked`, `unstable`, `draft`.
    NotClean(&'a str),
    /// A layer below has been asked to change (gh#289), naming its pull request.
    /// Nothing about *this* pull request is wrong; the ground under it is moving,
    /// so neither its diff nor GitHub's verdict on it is worth acting on yet.
    ChangesBelow(i64),
    /// Nobody has asked GitHub. Mergeability costs a call per open PR and rides
    /// the full sweep, so this is the ordinary state of a freshly-seen row.
    Unknown,
}

impl Landing<'_> {
    /// How the wire spells it — [`TaskRow::landing`]'s three values, and `None`
    /// for the unknown that is written as an absent field.
    pub fn as_str(&self) -> Option<&'static str> {
        match self {
            Landing::Ready { .. } => Some("ready"),
            Landing::CleanAgainstBase { .. } => Some("waiting-on-stack"),
            Landing::NotClean(_) => Some("not-clean"),
            Landing::ChangesBelow(_) => Some("changes-below"),
            Landing::Unknown => None,
        }
    }

    /// Would merging this pull request land it? The one-bit answer, for a
    /// caller that wants the AND and not the reason.
    pub fn ready(&self) -> bool {
        matches!(self, Landing::Ready { .. })
    }
}

/// The half of a pull request this vocabulary reads (gh#389).
///
/// Every function below began as a function on [`TaskRow`], because the board's
/// list was the only surface that drew a stack. The review screen is the other
/// one, and what it holds is an `AttemptReview` — a different shape about the
/// same pull request. Two implementations of "can this land" is precisely what
/// this vocabulary exists to prevent, so the functions read these five facts
/// and each shape says how it spells them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stacked<'a> {
    /// The task id, which is how a row finds *itself* in its own map.
    pub id: &'a str,
    /// GitHub's stack object, with the siblings the board can see joined onto
    /// it. `None` on a pull request that is not a layer of one.
    pub stack: Option<&'a RowStack>,
    /// The branch this pull request merges into — trunk for a standalone one,
    /// the layer below mid-stack.
    pub base_ref: Option<&'a str>,
    /// GitHub's `mergeable_state` for this pull request **alone**, which is why
    /// none of these functions renders it unqualified.
    pub mergeable: Option<&'a str>,
    /// The pull request number of the nearest open layer below that has been
    /// asked to change (gh#289).
    pub changes_below: Option<i64>,
}

impl<'a> From<&'a TaskRow> for Stacked<'a> {
    fn from(row: &'a TaskRow) -> Self {
        Stacked {
            id: &row.id,
            stack: row.stack.as_ref(),
            base_ref: row.pr_base_ref.as_deref().filter(|b| !b.is_empty()),
            mergeable: row.pr_mergeable.as_deref().filter(|s| !s.is_empty()),
            changes_below: row.changes_below,
        }
    }
}

/// Can this pull request land? (gh#283.)
///
/// The single implementation of the AND: the board fills
/// [`TaskRow::landing`] with it so `list --json` carries the verdict, and every
/// viewport calls it for the words. A caller holding a row that came off the
/// wire can call it too — it reads nothing but the row.
pub fn landing<'a>(row: impl Into<Stacked<'a>>) -> Landing<'a> {
    let row = row.into();
    // Ahead of GitHub's own answer, and ahead of "nobody has asked": a layer
    // below that has been asked to change is going to be rewritten, and every
    // fact about this pull request measured against the branch underneath it —
    // `mergeable_state` included — is about to be measured against a different
    // branch. Saying `clean` here is the one answer that gets somebody to press
    // merge, or worse, to approve (gh#289).
    if let Some(number) = row.changes_below {
        return Landing::ChangesBelow(number);
    }
    let Some(state) = row.mergeable.filter(|s| !s.is_empty()) else {
        return Landing::Unknown;
    };
    if state != MERGEABLE_CLEAN {
        return Landing::NotClean(state);
    }
    let Some(stack) = row.stack else {
        return Landing::Ready { below: 0 };
    };
    let seen = stack.below(row.id);
    // A layer that already landed is nobody's obstacle — which is exactly the
    // child whose parent merged and whose own PR GitHub retargeted onto trunk.
    let below: Vec<&StackLayer> = seen.iter().filter(|l| l.open).collect();
    if let Some(blocker) = below
        .iter()
        .find(|l| l.mergeable.as_deref().is_some_and(|m| m != MERGEABLE_CLEAN))
    {
        return Landing::CleanAgainstBase {
            blocker: Some(blocker),
        };
    }
    if below.iter().any(|l| l.mergeable.is_none()) {
        return Landing::CleanAgainstBase { blocker: None };
    }
    // Does the board hold every layer GitHub says is underneath this one? A
    // stack reaching into a repository nobody polls is a map with holes, and a
    // hole under you is not a clean parent — the count is GitHub's for exactly
    // this reason. Without a position there is nothing to check against, and
    // "ready" is not the answer to give when the topology is the part in doubt.
    let complete = stack
        .position
        .is_some_and(|p| seen.len() as i64 >= (p - 1).max(0));
    if !complete {
        return Landing::CleanAgainstBase { blocker: None };
    }
    Landing::Ready { below: below.len() }
}

/// The branch a pull request's `mergeable_state` was measured against, for
/// wording that has to name it. Falls back to a phrase rather than a branch:
/// "conflicts with its base" is still true when the board never learned which
/// branch that is.
fn base_of<'a>(row: Stacked<'a>) -> &'a str {
    row.base_ref.filter(|b| !b.is_empty()).unwrap_or("its base")
}

/// How a layer of a stack is named in a sentence: its pull request number where
/// there is one, else the row's identifier.
fn layer_name(layer: &StackLayer) -> String {
    match layer.pr_number {
        Some(n) => format!("PR #{n}"),
        None => layer.identifier.clone(),
    }
}

/// How a layer is named in a *map*, where the word `PR` is already implied by
/// every other entry beside it: `#12`, or the row's identifier for a layer that
/// has no pull request number yet.
pub fn layer_label(layer: &StackLayer) -> String {
    match layer.pr_number {
        Some(n) => format!("#{n}"),
        None => layer.identifier.clone(),
    }
}

/// What the board says about whether this pull request can land — the sentence
/// [`landing`] earns.
///
/// `None` when nobody has asked GitHub yet: a blank is honest and an invented
/// verdict is not. Every other answer names the branch it is measured against,
/// because that is the fact the flat `mergeable_state` was missing.
pub fn landing_note<'a>(row: impl Into<Stacked<'a>>) -> Option<String> {
    let row = row.into();
    let base = base_of(row);
    Some(match landing(row) {
        Landing::Unknown => return None,
        Landing::Ready { below: 0 } => "ready to land".to_string(),
        Landing::Ready { below } => format!("ready to land with {below} below"),
        Landing::CleanAgainstBase {
            blocker: Some(layer),
        } => {
            format!("clean against {base} · waiting on {}", layer_name(layer))
        }
        Landing::CleanAgainstBase { blocker: None } => format!("clean against {base}"),
        // Not "waiting on", which is what a dirty parent earns: this one is
        // going to be *rebased*, and the word has to say that the diff moves
        // rather than that a merge is queued behind something.
        Landing::ChangesBelow(number) => {
            format!("PR #{number} below was asked to change · this rebases under it")
        }
        Landing::NotClean(state) => match state {
            "behind" => format!("behind {base}"),
            "dirty" => format!("conflicts with {base}"),
            "blocked" => "blocked on a check or review".to_string(),
            "unstable" => "a check is failing".to_string(),
            "draft" => "still a draft".to_string(),
            // GitHub's preview may grow a state we have no words for. Say the
            // word it used against the branch it used it about, rather than
            // swallowing an objection we do not recognise.
            other => format!("{other} against {base}"),
        },
    })
}

/// Where this row sits in its stack, short enough for a list: `2 of 5`.
///
/// `None` on a row that is not part of one. GitHub's own count, so a stack
/// whose lower layers are outside the board's repos still reads its true size.
pub fn stack_note<'a>(row: impl Into<Stacked<'a>>) -> Option<String> {
    let stack = row.into().stack?;
    Some(match (stack.position, stack.size) {
        (Some(p), Some(size)) => format!("{p} of {size}"),
        (Some(p), None) => format!("layer {p}"),
        // GitHub said this PR is in a stack and nothing else about where. The
        // fact that it is stacked is the half that changes what `clean` means,
        // so it is still worth a word.
        (None, _) => "stacked".to_string(),
    })
}

/// The stack, in full, for a detail surface: `stack 2 of 5 · onto
/// board/gh-11-lexer · lands on main`.
///
/// The middle fact appears only when it differs from the last one — for the
/// bottom layer the branch it targets *is* where the stack lands, and saying
/// that twice reads as two different branches.
pub fn stack_line<'a>(row: impl Into<Stacked<'a>>) -> Option<String> {
    let row = row.into();
    let stack = row.stack?;
    let mut parts = vec![format!("stack {}", stack_note(row)?)];
    let target = stack.base_ref.as_deref().filter(|b| !b.is_empty());
    if let Some(base) = row.base_ref.filter(|b| !b.is_empty())
        && Some(base) != target
    {
        parts.push(format!("onto {base}"));
    }
    if let Some(target) = target {
        parts.push(format!("lands on {target}"));
    }
    Some(parts.join(" · "))
}

/// What a merge confirmation names, beyond the five facts [`Stacked`] carries
/// (gh#408).
///
/// The confirmation is the one sentence that has to say *which work* is about
/// to land, not only whether it can — so it reads the row's name and address on
/// top of the stack facts. Two surfaces spell it: the board's list holds a
/// [`TaskRow`], the review screen holds an `AttemptReview`, and both feed this
/// shape so the sentence a reader confirms cannot depend on which screen they
/// pressed the key on.
#[derive(Debug, Clone, Copy)]
pub struct MergeSubject<'a> {
    /// The row's name — `gh#353`, `LIN-142` — which is what the reader knows
    /// the work as (gh#357).
    pub identifier: &'a str,
    /// The pull request that merges. The confirmation still works without one
    /// — the engine will refuse the merge itself — but it cannot name the
    /// address.
    pub pr_number: Option<i64>,
    /// Whether the row *is* its pull request (gh#344's adopted rows). For
    /// those, naming the row and naming the pull request are the same act, and
    /// the sentence says one instead of both.
    pub is_pull_request: bool,
    /// The stack facts, spelled the way every landing derivation reads them.
    pub stacked: Stacked<'a>,
}

impl<'a> From<&'a TaskRow> for MergeSubject<'a> {
    fn from(row: &'a TaskRow) -> Self {
        MergeSubject {
            identifier: &row.identifier,
            pr_number: row.pr_number,
            is_pull_request: row.is_pull_request(),
            stacked: row.into(),
        }
    }
}

/// What a merge confirmation has to say before the board takes the one action
/// it cannot undo (gh#290).
///
/// The fact a confirm dialog exists to carry: merging a layer of a stack is not
/// merging a pull request. GitHub merges — or queues — every layer up to and
/// including the one asked for, as one atomic group, so a reader confirming on
/// the third of five is confirming three pull requests. The board knows which
/// three (gh#283 grouped them) and names them, because "and the 2 below it" is
/// exactly the kind of phrasing that gets read as "and some other things".
///
/// Only the layers *below* appear: they are the ones that come along. The two
/// above are untouched by this merge, and mentioning them would suggest
/// otherwise. Already-merged layers are left out for the same reason — they are
/// history in the chain, not cargo.
///
/// The board's own reservations come last and only when it has one. A reader who
/// opened the confirm on a pull request the board does not think can land is the
/// reader most in need of the sentence, and GitHub evaluates the rules at
/// execution time rather than at submission, so nothing upstream will stop them.
pub fn merge_confirmation<'a>(row: impl Into<MergeSubject<'a>>) -> String {
    let subject = row.into();
    // Named, then located (gh#357). The reader confirming this opened it from a
    // row called `gh#353`, and a dialog that answers "merge PR #13" is asking
    // them to match up two numbers before they can tell whether it is the same
    // work. The pull request still has to appear — it is what merges — but as
    // the address, in parentheses after the name.
    let what = match subject.pr_number {
        Some(n) if !subject.is_pull_request => format!("{} (PR #{n})", subject.identifier),
        _ => subject.identifier.to_string(),
    };
    // Where the merge lands: for a stack that is the stack's target, which is
    // trunk, and not this layer's base — the branch below it disappears into
    // the group merge.
    let row = subject.stacked;
    let target = row
        .stack
        .and_then(|s| s.base_ref.as_deref())
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| base_of(row));
    let below: Vec<&StackLayer> = row
        .stack
        .map(|s| s.below(row.id))
        .unwrap_or_default()
        .iter()
        .filter(|l| l.open)
        .collect();

    let mut parts = vec![format!("merge {what} into {target}")];
    if !below.is_empty() {
        let names: Vec<String> = below.iter().map(|l| layer_name(l)).collect();
        parts.push(format!(
            "this lands {} with it — GitHub merges the group or none of it",
            names.join(", ")
        ));
    }
    // `Ready` is the one verdict that adds nothing here: the sentence above
    // already said what merging does, and repeating "ready to land with 2
    // below" after naming the two is noise.
    if !landing(row).ready()
        && let Some(note) = landing_note(row)
    {
        parts.push(note);
    }
    parts.join(" · ")
}

/// The stack map a detail surface draws, bottom layer first — one entry per
/// sibling the board can see, this row included.
///
/// Empty for a row that is not stacked, so a surface can call it
/// unconditionally and draw nothing.
pub fn stack_map<'a>(row: impl Into<Stacked<'a>>) -> &'a [StackLayer] {
    row.into()
        .stack
        .map_or(&[], |stack| stack.layers.as_slice())
}

/// Which way a stack merges, said as a fact about *this* layer (gh#389).
///
/// The one sentence a reviewer needs that no per-pull-request fact carries.
/// GitHub merges a stack bottom-up: a layer cannot land before the ones under
/// it, and the layers over it land only after this one does. Both halves are
/// invisible on a screen that has been handed a single pull request — which is
/// the state the review screen was in until this issue — and getting the order
/// wrong is the one mistake in a stack that is both easy and destructive.
///
/// The rule is named once, at the front, and then said in this layer's own
/// numbers. Only *open* layers appear: a landed one is history in the chain
/// rather than something still to be sequenced.
///
/// `None` for a pull request that is not a layer, and for a lone layer with
/// nothing either side of it — there is no order to get wrong.
pub fn merge_order<'a>(row: impl Into<Stacked<'a>>) -> Option<String> {
    let row = row.into();
    let stack = row.stack?;
    let open = |layers: &[StackLayer]| -> Vec<String> {
        layers.iter().filter(|l| l.open).map(layer_label).collect()
    };
    let below = open(stack.below(row.id));
    let above = open(stack.above(row.id));
    // One pull request lands, two land. The list is short and a reader is
    // reading a sentence, not a table.
    let lands = |layers: &[String]| match layers.len() {
        1 => "lands",
        _ => "land",
    };
    Some(match (below.is_empty(), above.is_empty()) {
        (true, true) => return None,
        // The bottom layer. Nothing is in its way, and saying only that would
        // leave out the half that is about to depend on it.
        (true, false) => format!(
            "bottom-up: this is the bottom open layer — {} {} after it",
            above.join(", "),
            lands(&above),
        ),
        (false, true) => format!(
            "bottom-up: {} {} before this one",
            below.join(", "),
            lands(&below),
        ),
        (false, false) => format!(
            "bottom-up: {} {} before this one, {} after",
            below.join(", "),
            lands(&below),
            above.join(", "),
        ),
    })
}

// ---------------------------------------------------------------------------
// Whose subscription a run spends (gh#101)
// ---------------------------------------------------------------------------
//
// The billing guard's whole vocabulary lives here, for the same reason the
// section order does: four surfaces say it — the desktop picker, the TUI
// picker, the `comet-board` CLI and the upstream comment the board writes — and
// a warning worded three ways is three warnings nobody recognises as the same
// one. `comet-board` owns the *policy* (the `billing_guard` mode and the
// refusal); these are the words and the comparison.

/// The email whose subscription a dispatch would spend, out of the logins a
/// device has saved.
///
/// `slot` is the agent-account id the dispatch names — the route's, or the
/// picker's override. `None` is the box's own CLI login, which is the *active*
/// account for that harness: the same login a run with no slot at all would
/// reach, so naming it is describing what will happen rather than guessing.
///
/// `None` back means the device cannot name one — no such slot, no live login,
/// or a login whose credentials it could not read far enough to find an email.
/// Nothing accuses anybody on the strength of that: [`cross_billed`] is false
/// whenever this is `None`.
pub fn billed_email<'a>(
    accounts: &'a [crate::AgentAccount],
    harness: crate::HarnessId,
    slot: Option<&str>,
) -> Option<&'a str> {
    let account = match slot.filter(|s| !s.is_empty()) {
        Some(slot) => accounts
            .iter()
            .find(|a| a.harness == harness && a.id == slot)?,
        None => accounts.iter().find(|a| a.harness == harness && a.active)?,
    };
    account.email.as_deref().filter(|e| !e.is_empty())
}

/// Is this run spending somebody else's subscription?
///
/// The match is dispatcher-vs-slot-email and nothing more. How much the
/// dispatcher's name is worth is a separate question with its own answer
/// ([`TaskRow::dispatched_by_verified`]): on the box it is the frontend's
/// claim, over the relay it is the identity the edge verified, and this
/// comparison is the same either way. Two unknowns read as "not cross-billed"
/// rather than as an accusation — an unattributed dispatch (the bare CLI, an
/// orchestrating agent) names nobody to have wronged.
pub fn cross_billed(billed_to: Option<&str>, dispatcher: Option<&str>) -> bool {
    let (Some(billed), Some(by)) = (email(billed_to), email(dispatcher)) else {
        return false;
    };
    !billed.eq_ignore_ascii_case(by)
}

/// The phrase a `require-own` refusal carries, so a frontend can tell "the box
/// minds who pays for this" from every other reason a dispatch failed and offer
/// the confirm instead of a dead end.
///
/// A shared constant rather than each side's own substring: the refusal is
/// written by `comet-board` and read by the desktop panel, and a reworded
/// message that quietly stopped matching would turn the confirm into an error
/// nobody could act on.
pub const REQUIRE_OWN_REFUSAL: &str = "billing_guard = \"require-own\"";

/// How a surface names the human who released an attempt, with the strength of
/// the name on it (gh#161).
///
/// One derivation rather than each surface's own parenthetical, for the reason
/// the billing words are shared: "ana@example.com" and "ana@example.com, as
/// claimed" are different assertions, and a board that made them
/// interchangeable would be publishing the weaker one as the stronger. The mark
/// goes on the *claim*, not on the verified name, because the verified name is
/// what the sentence would ordinarily mean.
pub fn dispatcher_label(user: &str, verified: bool) -> String {
    let user = user.trim();
    if verified {
        user.to_string()
    } else {
        format!("{user} (as claimed)")
    }
}

/// A non-empty, trimmed email, or nothing.
fn email(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

/// What a picker row says about a selection that bills somebody else — the
/// short form, because it rides beside a chip that is already naming a login.
pub fn bills_label(billed_to: &str) -> String {
    format!("bills {}", billed_to.trim())
}

/// The one line a CLI prints, and a refusal leads with, before a cross-billed
/// release. Names the subscription (`Claude`, `Codex`) rather than the harness
/// id, because the person about to be charged thinks of it by the product's
/// name and not by comet's runtime spelling.
pub fn bills_warning(billed_to: &str, harness: crate::HarnessId) -> String {
    format!(
        "this run bills {}'s {} — pass --account <your slot>",
        billed_to.trim(),
        subscription_noun(harness)
    )
}

/// What the upstream dispatch comment appends when the run is cross-billed, so
/// the record is public to both parties rather than living on one usage page.
pub fn bills_comment_suffix(billed_to: &str) -> String {
    format!(" · on {}'s subscription", billed_to.trim())
}

/// The subscription a harness spends, named the way its owner would.
pub fn subscription_noun(harness: crate::HarnessId) -> &'static str {
    match harness {
        crate::HarnessId::ClaudeCode => "Claude",
        crate::HarnessId::Codex => "Codex",
        crate::HarnessId::Cursor => "Cursor",
        crate::HarnessId::Opencode => "OpenCode",
        crate::HarnessId::Mock => "mock",
    }
}

/// What a board row says about who it is charging, for the life of the attempt
/// — `None` when nobody is being charged for somebody else.
///
/// Derived from the row alone, which is what lets both viewports show it
/// without asking the box anything: the attempt recorded whose subscription it
/// spends, and it recorded who said they released it.
pub fn billing_note(row: &TaskRow) -> Option<String> {
    let billed = row.billed_to.as_deref()?;
    cross_billed(Some(billed), row.dispatched_by_user.as_deref()).then(|| bills_label(billed))
}

// ---------------------------------------------------------------------------
// How full a live agent's context is (gh#271)
// ---------------------------------------------------------------------------

/// Below this share of the window, a row says nothing about its context.
///
/// Not squeamishness about clutter: a working row already carries a runtime, a
/// workspace and an elapsed clock, and a gauge that reads `ctx 8%` on every
/// row for the first hour teaches the eye to skip the place the warning will
/// appear. Half a window is where the number starts predicting something.
pub const CONTEXT_NOTE_FLOOR: f64 = 0.5;

/// Absent a threshold the harness itself names, the share at which a row is
/// called near compaction. Codex is the case: it states a window but no
/// compaction point (it fails the turn instead), so a ratio is all there is.
pub const CONTEXT_NEAR_COMPACTION: f64 = 0.9;

/// What a row says about its agent's context window — `ctx 62%`, or
/// `ctx 92% compacting` once the harness's own threshold is passed.
///
/// `None` for the two very different silences the board must not conflate: no
/// reading at all (a harness that meters no window, a CLI too old to answer),
/// and a reading with plenty of room left. Neither is worth a column, and
/// neither may be rendered as `0%`.
pub fn context_note(context: Option<crate::ContextUsage>) -> Option<String> {
    let context = context?;
    let percent = context.percent()?;
    if context.is_near_compaction(CONTEXT_NEAR_COMPACTION) {
        // The loud form. What it warns about is not the run ending — it is the
        // agent losing the context it has been working from, mid-task.
        return Some(format!("ctx {percent}% compacting"));
    }
    (context.fraction()? >= CONTEXT_NOTE_FLOOR).then(|| format!("ctx {percent}%"))
}

fn ws(row: &TaskRow) -> String {
    row.workspace
        .as_deref()
        .map(|w| format!("ws:{w}"))
        .unwrap_or_default()
}

/// Pad or truncate to an exact column width.
fn fixed(s: &str, width: usize) -> String {
    let t = truncate(s, width);
    let n = t.chars().count();
    format!("{t}{}", " ".repeat(width.saturating_sub(n)))
}

/// Truncate to `max` display cells, marking the cut with `…`.
pub fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// The detail surface — a row is a door (gh#132)
// ---------------------------------------------------------------------------
//
// A board row shows a truncated title and nothing else you can open. gh#125
// made the cursor's row wrap to a second line to answer "which Signicat issue",
// which bought half a sentence at the price of the list reflowing under the
// pointer. The answer to both is the same: rows never change height, and the
// full text lives one deliberate keypress away, on a surface that also holds
// the body, the labels, the history and the links.
//
// Everything the three surfaces say about that surface is derived here — what
// it can do with the row, what its history line reads, which links it offers —
// for the reason every other derivation is here: a detail panel that offers a
// Retry the TUI does not, or bills somebody in different words, is three
// features rather than one.

/// The issue text behind a row, fetched on demand (`ReadBoardTask`).
///
/// Deliberately *not* a field on [`TaskRow`]: `WatchBoard` republishes every
/// row on every sync cycle, and a hundred issue bodies is a hundred kilobytes
/// per frame relayed to a phone to render one truncated line. The body is read
/// exactly when somebody opens a row, and only that row's.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskDetail {
    /// The task this answers for, so a reply that raced a newer selection is
    /// dropped rather than shown under the wrong row.
    pub id: String,
    /// The issue's markdown body. `None` and empty mean the same thing: the
    /// issue has no description, which is a fact worth rendering as one.
    #[serde(default)]
    pub body: Option<String>,
}

/// What a detail surface says where the body would be, for an issue that has
/// none. A blank panel reads as a failed fetch; this reads as an empty issue.
pub const NO_BODY: &str = "No description on the issue.";

/// One thing a surface offers to do with a row.
///
/// The set is closed and shared so the peek panel, the TUI's full-screen detail
/// and the iOS sheet offer the same verbs on the same rows — and so the desktop
/// row's own chips, which have had this rule hard-coded since gh#49, are drawn
/// from it rather than beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowAction {
    /// Release a ready task. The account picker rides this on every surface.
    Dispatch,
    /// Release again. On an agent-blocked row this ends the live attempt first.
    Retry,
    /// End the live attempt. The issue stays open.
    Cancel,
    /// Open the chat the attempt is running in.
    OpenChat,
    /// Open the issue upstream.
    OpenIssue,
    /// Open the pull request the attempt raised.
    OpenPr,
}

impl RowAction {
    /// The words every surface uses for it. One spelling, so an operator who
    /// learned "Retry" on the desktop finds "Retry" on the phone.
    pub fn label(self) -> &'static str {
        match self {
            RowAction::Dispatch => "Dispatch",
            RowAction::Retry => "Retry",
            RowAction::Cancel => "Cancel",
            RowAction::OpenChat => "Open chat",
            RowAction::OpenIssue => "Open issue",
            RowAction::OpenPr => "Open PR",
        }
    }

    /// The spelling for a chip riding beside a truncated title, where the full
    /// label would crowd the thing it is an affordance for.
    ///
    /// Short, not different: an action a surface calls "Open PR" in one place
    /// and "PR" in another is still one action, but one it calls "Retry" here
    /// and "Redispatch" there is two.
    pub fn short_label(self) -> &'static str {
        match self {
            RowAction::OpenChat => "Open",
            RowAction::OpenIssue => "Issue",
            _ => self.label(),
        }
    }

    /// The verb as a sentence wants it: "enter to open PR", "double-click to
    /// dispatch". Lower case except where the word is not a word — a footer
    /// that lower-cased "PR" would be spelling it a fourth way.
    pub fn verb(self) -> &'static str {
        match self {
            RowAction::Dispatch => "dispatch",
            RowAction::Retry => "retry",
            RowAction::Cancel => "cancel",
            RowAction::OpenChat => "open chat",
            RowAction::OpenIssue => "open issue",
            RowAction::OpenPr => "open PR",
        }
    }

    /// Does this end somebody's work? Surfaces that colour a destructive action
    /// differently (all three do) ask here rather than each keeping a list.
    pub fn destructive(self) -> bool {
        matches!(self, RowAction::Cancel)
    }

    /// Does this release an agent, and so need the account picker first?
    ///
    /// The picker is not decoration: a dispatch spends somebody's subscription
    /// (gh#74), and a surface that skipped it would be the one place on the
    /// board where nobody is asked.
    pub fn releases(self) -> bool {
        matches!(self, RowAction::Dispatch | RowAction::Retry)
    }
}

/// The actions a row's own affordances offer — the desktop chips, the TUI's
/// keys, the phone row's one chip.
///
/// Exactly the rule the desktop panel has drawn since gh#49, lifted so the
/// other surfaces stop re-deriving it: a ready row releases, a live one opens
/// and cancels, a blocked one may also retry (which replaces its attempt), a
/// review offers its PR, and a failed attempt is retried or cleared away. An
/// unroutable row offers no release on any surface — `dispatch` would refuse it.
pub fn row_actions(row: &TaskRow) -> Vec<RowAction> {
    let mut out = Vec::new();
    match row.state() {
        BoardState::Ready => {
            if row.dispatchable {
                out.push(RowAction::Dispatch);
            }
        }
        BoardState::Working => {
            out.push(RowAction::OpenChat);
            out.push(RowAction::Cancel);
        }
        BoardState::Blocked => {
            if row.dispatchable {
                out.push(RowAction::Retry);
            }
            out.push(RowAction::OpenChat);
            out.push(RowAction::Cancel);
        }
        BoardState::Review => {
            if row.pr_url.as_deref().is_some_and(|u| !u.is_empty()) {
                out.push(RowAction::OpenPr);
            }
        }
        BoardState::Failed => {
            if row.dispatchable {
                out.push(RowAction::Retry);
            }
            out.push(RowAction::Cancel);
        }
        BoardState::Done => {}
    }
    out
}

/// The one verb a row wears without being asked for it — and the one `enter`
/// runs on it (gh#176).
///
/// [`row_actions`] says which verbs a row *has*; nothing said which of them was
/// the row's own. Every surface then picked for itself: the desktop's `enter`
/// arm, the TUI's key table, the phone's single chip. Three answers to one
/// question, and for a blocked row three *different* answers — its action list
/// leads with `Retry`, but `enter` opens the chat, because a blocked agent is
/// alive and waiting for you rather than needing replacing (gh#49).
///
/// So the designation lives here, beside the set it selects from, and the
/// answer is always a member of that set: a surface that draws this chip and
/// binds `enter` to it cannot end up offering a verb the board would refuse.
///
/// `None` where a row has nothing to be done to it — a closed row, a ready one
/// with no route, a review with no PR raised. A permanently visible verb is
/// worth having because it is always true; inventing one for those rows would
/// spend the same space on a lie.
pub fn primary_action(row: &TaskRow) -> Option<RowAction> {
    let wanted = match row.state() {
        BoardState::Ready => RowAction::Dispatch,
        // A failed attempt's verb is the one that replaces it (gh#42).
        BoardState::Failed => RowAction::Retry,
        BoardState::Working | BoardState::Blocked => RowAction::OpenChat,
        BoardState::Review => RowAction::OpenPr,
        BoardState::Done => return None,
    };
    row_actions(row)
        .into_iter()
        .find(|action| *action == wanted)
}

/// The rest — the verbs a surface may keep behind a hover or a long press.
///
/// Order is [`row_actions`]', minus the primary. Same set, so nothing a row can
/// do goes missing by being neither primary nor secondary.
pub fn secondary_actions(row: &TaskRow) -> Vec<RowAction> {
    let primary = primary_action(row);
    row_actions(row)
        .into_iter()
        .filter(|action| Some(*action) != primary)
        .collect()
}

/// Everything a *detail* surface offers: the row's own actions, plus the links
/// a list has no room for.
///
/// The links come last and in one order — PR before issue, because a row that
/// has a PR is a row whose PR is the thing you came to read. A row's actions
/// are never dropped here: the detail is for reading, but nothing you could do
/// from the list should require going back to it.
pub fn detail_actions(row: &TaskRow) -> Vec<RowAction> {
    let mut out = row_actions(row);
    if row.pr_url.as_deref().is_some_and(|u| !u.is_empty()) && !out.contains(&RowAction::OpenPr) {
        out.push(RowAction::OpenPr);
    }
    if !row.url.is_empty() {
        out.push(RowAction::OpenIssue);
    }
    out
}

/// Is there anything to review on this row (§gh#180, §gh#344)?
///
/// One attempt is enough, and the state is deliberately not consulted. A
/// finished attempt, a failed one and a cancelled one all left a branch and a
/// run journal behind them, and "what did it actually change before it stopped"
/// is the same question in all three cases — arguably the most useful one on a
/// row that failed.
///
/// **A pull request is enough on its own.** A row whose pull request nobody
/// dispatched — an agent that did the work in its own chat instead of releasing
/// it, or a person who pushed a branch — has no attempt and is still the work
/// that most needs reading, precisely because no process watched it happen. The
/// diff is on GitHub, the claims are empty and therefore the whole diff is
/// unaccounted for, and saying that is worth a door. Barring one would send the
/// row `review` → `done` having never been reviewable.
///
/// What has no answer is a row with neither: nothing ran and nothing was
/// pushed, so there is no diff, no claims and no journal, and a surface that
/// offered the door anyway would be offering an empty room.
///
/// Not a [`RowAction`]: the action set is the verbs the *board* offers, and
/// they must mean the same thing on every surface that draws them. Reviewing is
/// a place a surface may or may not have, so the rule about which rows have one
/// lives here and the affordance stays each surface's own.
pub fn reviewable(row: &TaskRow) -> bool {
    row.attempts > 0 || row.pr_url.as_deref().is_some_and(|url| !url.is_empty())
}

/// The URL an action opens, or `None` for the ones that are not links.
pub fn action_url(row: &TaskRow, action: RowAction) -> Option<&str> {
    match action {
        RowAction::OpenIssue => Some(row.url.as_str()).filter(|u| !u.is_empty()),
        RowAction::OpenPr => row.pr_url.as_deref().filter(|u| !u.is_empty()),
        _ => None,
    }
}

/// What has been tried on this row: `attempt 2 · last failed 3h ago · bills
/// brede@tally.no`.
///
/// The one line the list has never had room for and the reason a detail surface
/// earns its space — a `ready` row that has already failed twice is a different
/// row from one nobody has touched, and the board draws them identically.
///
/// `None` on a row nothing has ever run on: "attempt 0" is not a fact, it is a
/// blank where a fact would go.
pub fn history_line(row: &TaskRow, now: DateTime<Utc>) -> Option<String> {
    if row.attempts == 0 {
        return None;
    }
    let mut parts = vec![format!("attempt {}", row.attempts)];
    if let Some(outcome) = row.last_outcome.as_deref().filter(|o| !o.is_empty()) {
        let when = row
            .last_outcome_at
            .as_deref()
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
            .map(|at| format_age((now - at.with_timezone(&Utc)).num_seconds()));
        parts.push(match when {
            Some(age) => format!("last {outcome} {age} ago"),
            None => format!("last {outcome}"),
        });
    }
    // gh#34's honesty field: the board closed this attempt and then found its
    // agent still working. Rare, and the kind of thing you only ever want to
    // read about the row you are already looking at.
    if row.reopened > 0 {
        parts.push(format!("reopened {}×", row.reopened));
    }
    // Whose subscription it spent, in the same words the pickers and the
    // upstream comment use. Unconditional here, unlike the row's own sub-line
    // ([`billing_note`], which speaks up only when somebody else is paying):
    // the detail is the place you come to *ask*, and an answer that appears
    // only when there is a problem cannot be trusted to mean anything.
    if let Some(billed) = row.billed_to.as_deref().filter(|b| !b.trim().is_empty()) {
        parts.push(bills_label(billed));
    }
    Some(parts.join(" · "))
}

/// Where this row's work is happening, for the detail's facts block: the route,
/// the space, the runtime and the branch, each named only when it is known.
///
/// The list says at most two of these, and only ever in the sub-line's fixed
/// columns; this is the same facts said in full, once, for the row you opened.
pub fn placement_line(row: &TaskRow) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(route) = row.route.as_deref().filter(|r| !r.is_empty()) {
        parts.push(route.to_string());
    } else {
        parts.push(NO_ROUTE.to_string());
    }
    if let Some(runtime) = row.runtime.as_deref().filter(|r| !r.is_empty()) {
        parts.push(runtime.to_string());
    }
    if let Some(workspace) = row.workspace.as_deref().filter(|w| !w.is_empty()) {
        parts.push(format!("ws:{workspace}"));
    }
    if let Some(branch) = row.branch.as_deref().filter(|b| !b.is_empty()) {
        parts.push(branch.to_string());
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

// ---------------------------------------------------------------------------
// Live agents — the Active group's board half (gh#103)
// ---------------------------------------------------------------------------
//
// herdr gave presence away: a working agent was a pane, and the pane list was
// the sidebar. Here a dispatched agent is a chat among chats, so tracking five
// of them meant opening the board pane or nothing. This is that list, rebuilt
// from what is already streamed — board rows joined to chats, state read live
// off the session watch. Pure presentation: nothing here dispatches, settles or
// decides anything, and a row leaves the list only because the attempt it names
// stopped being live.
//
// These rows drew under their own "Agents" heading until gh#123 folded them
// into the single Active group — see [`active_rows`], which owns the order.

/// What a live attempt's agent is doing right now.
///
/// Three states where the board has two, and the split is the point. The board
/// calls a dead run and an agent asking a question both `blocked` — correctly,
/// since both hold a chat and a concurrency slot — but they ask different
/// things of a human: one wants an answer, the other a retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    /// Waiting on a human: a question, or a permission prompt.
    Blocked,
    /// Its run died. Still a live attempt — a retry is the *same* attempt.
    Errored,
    Working,
}

impl AgentState {
    /// Sidebar order — lower is more urgent, and this is why blocked floats.
    ///
    /// The same ranking [`crate::view::attention_rank`] gives chat rows, for the
    /// same reason: a question outranks a corpse, which outranks work that is
    /// going fine on its own.
    pub fn rank(self) -> u8 {
        match self {
            AgentState::Blocked => 0,
            AgentState::Errored => 1,
            AgentState::Working => 2,
        }
    }

    /// Worth interrupting a human for — what the section's count badge counts.
    pub fn needs_attention(self) -> bool {
        !matches!(self, AgentState::Working)
    }

    /// The board's own glyphs, so a row means the same thing in the sidebar as
    /// it does in the board pane one keystroke away.
    pub fn glyph(self) -> &'static str {
        match self {
            AgentState::Blocked => BoardState::Blocked.glyph(),
            AgentState::Errored => BoardState::Failed.glyph(),
            AgentState::Working => BoardState::Working.glyph(),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AgentState::Blocked => "blocked",
            AgentState::Errored => "errored",
            AgentState::Working => "working",
        }
    }
}

/// One live attempt, as the sidebar draws it.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentRow {
    pub task_id: String,
    /// Where the click goes. Always a chat that exists — see [`agent_rows`].
    pub chat_id: String,
    /// The issue identifier (`AGE-14`, `gh#103`): what the agent is *for*, and
    /// a better title than the chat's, which the agent writes about itself.
    pub identifier: String,
    /// A short slug of the task's title (gh#364), to be drawn after the
    /// identifier — `gh#341 review-page-loads`.
    ///
    /// Four agents in flight is the case this exists for: `gh#341 gh#342
    /// gh#343 gh#356` is four rows that look alike, and the section's whole job
    /// is telling them apart at a glance. Its own field rather than folded into
    /// [`identifier`](Self::identifier) because it is **decoration on the key
    /// and drops first** — a renderer short of width draws the identifier
    /// without it, and one that had them pre-joined could only truncate, which
    /// eats the name from the wrong end.
    ///
    /// `None` when the title yields no content words; see
    /// [`crate::view::slug::title_slug`].
    pub slug: Option<String>,
    pub branch: Option<String>,
    pub state: AgentState,
    /// When the attempt started. `None` on a row carrying no `started_at`,
    /// which is a board that predates the field.
    ///
    /// The instant rather than the age, so a viewport can re-read the clock on
    /// its own frames instead of rebuilding this list once a second to move a
    /// counter — the same rule the spinners follow.
    pub started_at: Option<DateTime<Utc>>,
    /// The route's `max_duration`, when it has one.
    pub cap_secs: Option<u64>,
}

impl AgentRow {
    /// What to call this row where there is room for both — `gh#341
    /// review-page-loads`, and the bare identifier when there is no slug.
    ///
    /// A space, not a separator: `·` would make the slug read as a second
    /// field, and it is not one. It is the name wearing a description.
    ///
    /// For one-line surfaces and width arithmetic. A renderer that styles the
    /// two halves differently — the identifier in the row's own weight, the
    /// slug muted after it — reads the fields instead, and must drop the slug
    /// rather than truncate this string: what elides from the right here is the
    /// decoration's job to lose, never the identifier's.
    pub fn name(&self) -> String {
        match &self.slug {
            Some(slug) => format!("{} {slug}", self.identifier),
            None => self.identifier.clone(),
        }
    }

    /// How long this attempt has been going.
    pub fn elapsed_secs(&self, now: DateTime<Utc>) -> Option<i64> {
        agent_elapsed_secs(self.started_at, now)
    }

    /// Past its cap: gh#70's clock is warning it now and will cancel it next.
    pub fn over_cap(&self, now: DateTime<Utc>) -> bool {
        agent_over_cap(self.started_at, self.cap_secs, now)
    }

    /// `1h50m / 2h`, bare elapsed on an uncapped route, or nothing.
    pub fn elapsed_label(&self, now: DateTime<Utc>) -> Option<String> {
        agent_elapsed_label(self.started_at, self.cap_secs, now)
    }
}

// The same three answers as free functions, for a viewport that has flattened
// the row into its own model and holds the two fields rather than the struct.

/// Seconds since an attempt started. Never negative: clock skew must not read
/// as a count-up from the future.
pub fn agent_elapsed_secs(started_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<i64> {
    Some((now - started_at?).num_seconds().max(0))
}

/// Past the route's cap. The *decision* stays board-side
/// (`comet_board::overrun`) — this is the display's half, and it deliberately
/// says nothing about the grace, which is the board's to spend.
pub fn agent_over_cap(
    started_at: Option<DateTime<Utc>>,
    cap_secs: Option<u64>,
    now: DateTime<Utc>,
) -> bool {
    match (agent_elapsed_secs(started_at, now), cap_secs) {
        (Some(elapsed), Some(cap)) => elapsed as u64 >= cap,
        _ => false,
    }
}

/// `1h50m / 2h` — or bare elapsed where the route caps nothing, or nothing at
/// all where the row cannot say when it started.
pub fn agent_elapsed_label(
    started_at: Option<DateTime<Utc>>,
    cap_secs: Option<u64>,
    now: DateTime<Utc>,
) -> Option<String> {
    let elapsed = format_elapsed(agent_elapsed_secs(started_at, now)?);
    Some(match cap_secs {
        Some(cap) => format!("{elapsed} / {}", format_cap(cap)),
        None => elapsed,
    })
}

/// Every live attempt with a chat to open, most urgent first.
///
/// The three inputs are the three standing streams every viewport already
/// holds: `WatchBoard` rows, the chat rows, and the session watch.
///
/// - **A live attempt is `working` or `blocked` with a chat id.** That is the
///   whole membership rule, and it is why the row leaves on its own: settle,
///   cancel and orphan all end the attempt, which clears `chat_id` and moves
///   the row out of both states in the same frame. The chat stays findable
///   under its space, as it always was.
/// - **The chat must exist here.** A row whose chat has not synced (or is not
///   shared with this person) is dropped rather than drawn as something that
///   cannot be opened.
/// - **State comes from the session watch, not from the row.** The board's
///   state is a sync cycle old; the session mirror is live, and staleness-gated
///   ([`crate::view::effective_indicator`]) so a crashed backend cannot leave an
///   eternal spinner in the sidebar. The row's state is the fallback for a chat
///   with no session mirror yet — a dispatch whose first run has not started.
pub fn agent_rows(
    rows: &[TaskRow],
    chats: &[crate::Chat],
    sessions: &[crate::Session],
    now: DateTime<Utc>,
) -> Vec<AgentRow> {
    let mut out: Vec<AgentRow> = rows
        .iter()
        .filter(|row| row.state().holds_pane())
        .filter_map(|row| {
            let chat_id = row.chat_id.as_deref()?;
            let chat = chats.iter().find(|c| c.id == chat_id)?;
            let session = sessions.iter().find(|s| s.chat_id == chat_id);
            Some(AgentRow {
                task_id: row.id.clone(),
                chat_id: chat_id.to_string(),
                identifier: row.identifier.clone(),
                slug: row.slug(),
                // The chat's branch first: it is the checkout the agent is
                // actually in, and the attempt row's copy is what it was cut as.
                branch: chat
                    .branch
                    .as_deref()
                    .or(row.branch.as_deref())
                    .map(str::trim)
                    .filter(|b| !b.is_empty())
                    .map(str::to_string),
                state: agent_state(row.state(), session, now),
                started_at: row
                    .started_at
                    .as_deref()
                    .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
                    .map(|at| at.with_timezone(&Utc)),
                cap_secs: row.max_duration_secs,
            })
        })
        .collect();
    // Urgency first, then longest-running — which is stable, since that order is
    // start order and start order never changes under a viewer. A row that
    // cannot say when it started sorts last; the identifier breaks the final tie
    // so the sort is total.
    out.sort_by(|a, b| {
        a.state
            .rank()
            .cmp(&b.state.rank())
            .then_with(|| match (a.started_at, b.started_at) {
                (Some(a), Some(b)) => a.cmp(&b),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
            .then_with(|| a.identifier.cmp(&b.identifier))
    });
    out
}

/// What the session mirror says about this attempt, falling back to the board.
/// `pub(crate)` for the Needs-you inbox ([`crate::view::needs`]), which must
/// reach the same verdict about the same attempt.
pub(crate) fn agent_state(
    state: BoardState,
    session: Option<&crate::Session>,
    now: DateTime<Utc>,
) -> AgentState {
    match crate::view::effective_indicator(session, now) {
        crate::view::Indicator::Working => AgentState::Working,
        crate::view::Indicator::AwaitingInput => AgentState::Blocked,
        crate::view::Indicator::Errored => AgentState::Errored,
        // No live session: idle, stale, or never started. The board's verdict
        // is older but it is a verdict, and `blocked` is the one it reaches for
        // a run that ended without settling.
        crate::view::Indicator::None => match state {
            BoardState::Blocked => AgentState::Blocked,
            _ => AgentState::Working,
        },
    }
}

// ---------------------------------------------------------------------------
// Unmanaged runs — every working chat the board is NOT running (gh#117)
// ---------------------------------------------------------------------------
//
// [`agent_rows`] answers "what has the board released", which is a smaller
// question than "what is working on this box". An orchestrator that raised
// in-chat subagents instead of dispatching, an ad-hoc chat somebody started by
// hand: all of them are real runs holding a real checkout, and none of them
// has an attempt row, so the Agents section shows nothing and the only honest
// answer to "are they even alive" was `pgrep` over ssh. This is the other half
// of the list. (The pinned orchestrator, which used to lead this group, now
// has a fixed slot of its own — gh#122 — and is subtracted here.)
//
// The join is the same one gh#103 does, minus the board row — which is exactly
// why it is cheap: the session watch already streams a status for every chat,
// and a box hosting no board at all still fills this group.

/// What a chat with no title is called — the spelling all three sessions lists
/// already use, pinned here so an unmanaged row cannot drift from them.
///
/// The two lists are a few rows apart, and one untitled chat reading
/// `New session` in one and something else in the other looks like two chats.
pub const UNTITLED_CHAT: &str = "New session";

/// One working chat that no board attempt accounts for.
///
/// Deliberately thinner than [`AgentRow`]: there is no issue, no branch
/// promised, no cap and no attempt behind it, so the row says only what is
/// knowable — what the chat calls itself, how long its run has been going, and
/// whether it is waiting on a person.
#[derive(Debug, Clone, PartialEq)]
pub struct RunningRow {
    /// Where the click goes, and this row's identity.
    pub chat_id: String,
    /// The chat's own title ([`UNTITLED_CHAT`] when it has none). The agent
    /// wrote it about itself, which is second-best to an issue identifier and
    /// the best thing there is when no issue exists.
    pub title: String,
    /// The space to land in when the row is opened, as the chat records it.
    pub space_id: Option<String>,
    /// `Working` or `Blocked` and never `Errored` — membership is the live
    /// indicator, and an errored run is not a working one.
    pub state: AgentState,
    /// When the *run* started, off the session mirror — not when the chat was
    /// created, which for a long-lived orchestrator is days ago and says
    /// nothing about the work in front of it. `None` where the mirror carries
    /// no start (an engine that predates the field).
    ///
    /// The instant rather than the age, on the same rule [`AgentRow`] follows:
    /// a viewport re-reads the clock on its own frames instead of rebuilding
    /// this list once a second to move a counter.
    pub started_at: Option<DateTime<Utc>>,
}

impl RunningRow {
    pub fn elapsed_secs(&self, now: DateTime<Utc>) -> Option<i64> {
        agent_elapsed_secs(self.started_at, now)
    }

    /// Bare elapsed — nothing caps these runs, so there is no second number to
    /// read it against. `None` where the mirror cannot say when it started.
    pub fn elapsed_label(&self, now: DateTime<Utc>) -> Option<String> {
        agent_elapsed_label(self.started_at, None, now)
    }
}

/// Every working chat that is not a live board attempt, most urgent first.
///
/// - **Membership is the session watch and nothing else.** `Working` or
///   `AwaitingInput`, staleness-gated by [`crate::view::effective_indicator`],
///   so the group fills within one watch frame of a run starting and empties
///   within one of it stopping. No board row is consulted to get *in*.
/// - **A live attempt is subtracted, not re-drawn.** Any chat claimed by a
///   `working`/`blocked` row is [`agent_rows`]'s to draw — that half knows its
///   issue, branch and cap; showing it twice would double-count the box's
///   load. The subtraction reads the board rows directly rather than
///   [`agent_rows`]'s output, so a claimed chat stays out of this group even
///   in the case that drops it from the other one.
/// - **Archived is not a reason to hide a run.** Archiving is a decision about
///   a *finished* chat; one that is working anyway is the exact invisible run
///   this group exists to surface.
/// - **The pinned orchestrator is subtracted too** (gh#122): it has a fixed
///   slot of its own above Spaces, which carries its live state — a second
///   row here would report the same run twice.
///
/// A board is not required. `rows` empty — no board on this box, the host sweep
/// still running, a phone that has not attached — subtracts nothing, and the
/// group is the whole live list.
pub fn running_rows(
    rows: &[TaskRow],
    chats: &[crate::Chat],
    sessions: &[crate::Session],
    orchestrator: Option<&str>,
    now: DateTime<Utc>,
) -> Vec<RunningRow> {
    let dispatched: Vec<&str> = rows
        .iter()
        .filter(|row| row.state().holds_pane())
        .filter_map(|row| row.chat_id.as_deref())
        .collect();
    let mut out: Vec<RunningRow> = chats
        .iter()
        .filter(|chat| !dispatched.contains(&chat.id.as_str()))
        .filter(|chat| orchestrator != Some(chat.id.as_str()))
        .filter_map(|chat| {
            let session = sessions.iter().find(|s| s.chat_id == chat.id);
            let state = match crate::view::effective_indicator(session, now) {
                crate::view::Indicator::Working => AgentState::Working,
                crate::view::Indicator::AwaitingInput => AgentState::Blocked,
                // Errored and None are not runs. A dead chat is the sessions
                // list's to report, at the recency it earned.
                _ => return None,
            };
            Some(RunningRow {
                chat_id: chat.id.clone(),
                title: chat
                    .title
                    .as_deref()
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .unwrap_or(UNTITLED_CHAT)
                    .to_string(),
                space_id: chat.space_id.clone(),
                state,
                started_at: session.and_then(|s| s.started_at),
            })
        })
        .collect();
    // The same order the Agents group uses, for the same reason: a question
    // outranks work going fine, and under that, longest-running first is stable
    // because start order never changes under a viewer. The chat id breaks the
    // final tie so the sort is total — titles are not unique and change under
    // the reader as an agent renames its own chat.
    out.sort_by(|a, b| {
        a.state
            .rank()
            .cmp(&b.state.rank())
            .then_with(|| match (a.started_at, b.started_at) {
                (Some(a), Some(b)) => a.cmp(&b),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
            .then_with(|| a.chat_id.cmp(&b.chat_id))
    });
    out
}

// ---------------------------------------------------------------------------
// Active — the one group everything alive draws under (gh#123)
// ---------------------------------------------------------------------------
//
// [`agent_rows`] and [`running_rows`] split the live list by how a run started
// — the board released it, or somebody (or some orchestrator) just started it.
// That is a mechanism distinction, and the reader's question does not contain
// it: "what is working, and which of it wants me" has one answer, not two. So
// one group, needs-you first, then working, origin-blind in the order and
// visible on the row — an attempt wears its issue identifier and keeps its
// branch and cap, an unmanaged run is its own title and nothing more.

/// One row of the sidebar's Active group: a live board attempt, or a working
/// chat no attempt accounts for.
///
/// The two memberships partition by construction — [`running_rows`] subtracts
/// every chat a live attempt claims — so the union never draws a chat twice,
/// and merging them is only a matter of order.
#[derive(Debug, Clone, PartialEq)]
pub enum ActiveRow {
    /// The board released this: it has an issue, a branch, a cap and a bill.
    Agent(AgentRow),
    /// A run the board never heard of: the orchestrator, an ad-hoc chat,
    /// anything started by hand.
    Unmanaged(RunningRow),
}

impl ActiveRow {
    /// Where the click goes, and the row's identity — unique across both
    /// variants, because the partition never claims a chat twice.
    pub fn chat_id(&self) -> &str {
        match self {
            ActiveRow::Agent(row) => &row.chat_id,
            ActiveRow::Unmanaged(row) => &row.chat_id,
        }
    }

    pub fn state(&self) -> AgentState {
        match self {
            ActiveRow::Agent(row) => row.state,
            ActiveRow::Unmanaged(row) => row.state,
        }
    }

    pub fn started_at(&self) -> Option<DateTime<Utc>> {
        match self {
            ActiveRow::Agent(row) => row.started_at,
            ActiveRow::Unmanaged(row) => row.started_at,
        }
    }
}

/// Everything alive, most urgent first, blind to how it started.
///
/// The halves arrive pre-sorted, but concatenating them would put a working
/// attempt above a blocked hand-started run — the exact order the merge exists
/// to end — so the union is sorted once, by the key both halves already use:
/// urgency, then longest-running (stable, since start order never changes
/// under a viewer), then the chat id, which every row carries and no two rows
/// share.
pub fn active_rows(
    rows: &[TaskRow],
    chats: &[crate::Chat],
    sessions: &[crate::Session],
    orchestrator: Option<&str>,
    now: DateTime<Utc>,
) -> Vec<ActiveRow> {
    let mut out: Vec<ActiveRow> = agent_rows(rows, chats, sessions, now)
        .into_iter()
        .map(ActiveRow::Agent)
        .chain(
            // The pinned orchestrator is subtracted with the rest of the
            // unmanaged membership rules (gh#122): its slot above Spaces
            // carries its live state, and a second row here would report
            // the same run twice.
            running_rows(rows, chats, sessions, orchestrator, now)
                .into_iter()
                .map(ActiveRow::Unmanaged),
        )
        .collect();
    out.sort_by(|a, b| {
        a.state()
            .rank()
            .cmp(&b.state().rank())
            .then_with(|| match (a.started_at(), b.started_at()) {
                (Some(a), Some(b)) => a.cmp(&b),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
            .then_with(|| a.chat_id().cmp(b.chat_id()))
    });
    out
}

/// How many active rows want a human — the group header's count badge.
pub fn active_needing_attention(rows: &[ActiveRow]) -> usize {
    rows.iter()
        .filter(|row| row.state().needs_attention())
        .count()
}

// ---------------------------------------------------------------------------
// Which device hosts the board (gh#55)
// ---------------------------------------------------------------------------

/// Evidence that a board has ever been dispatched from: any row with an attempt
/// on record (gh#125).
///
/// This is what an automatic host sweep settles on. A board *service* answers
/// `WatchBoard` wherever `COMET_BOARD` is unset, so "delivered a frame" proves
/// only that a board exists — a laptop's stale test board answers as readily as
/// the box the org actually works from, and it answers *first* because the
/// sweep asks this device before the others. Dispatch is the difference: a
/// board somebody has released work from is the org's board; one that only ever
/// collected rows is furniture. The sweep therefore holds a frame with no
/// dispatch evidence as a *fallback* and keeps asking, settling on it only when
/// no candidate with evidence answers — so a lone device, and a genuinely
/// fresh install, still see their own board.
///
/// `attempts`, not `chat_id`: the chat id rides only the live attempt, and the
/// box's board between dispatches must not read as furniture.
pub fn board_dispatched(rows: &[TaskRow]) -> bool {
    rows.iter().any(|row| row.attempts > 0)
}

/// The devices a board pane tries, in order, when the operator has pinned none.
/// `None` means this device — no `targetDeviceId` passthrough.
///
/// The board store lives on exactly ONE device (docs/BOARD.md: one host device
/// is correct while one box hosts the board), and the board RPCs are
/// relay-forwardable, so a viewport that finds no board locally is not out of
/// options — it just has to ask the other devices. This device comes first
/// because asking locally is free; whether its answer *settles* the sweep is
/// [`board_dispatched`]'s question. The rest follow in registration order,
/// which is stable across heartbeats (the same order the device switcher lists
/// them in), so the sweep visits them the same way twice.
///
/// A candidate is ruled out by its `WatchBoard` stream ending without ever
/// delivering a frame — the engine refuses the subscription outright when it
/// hosts no board, so "said nothing at all" IS the answer.
pub fn host_candidates(
    devices: &[crate::Device],
    local_device_id: Option<&str>,
) -> Vec<Option<String>> {
    let mut others: Vec<&crate::Device> = devices
        .iter()
        .filter(|d| Some(d.id.as_str()) != local_device_id)
        .collect();
    others.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    std::iter::once(None)
        .chain(others.into_iter().map(|d| Some(d.id.clone())))
        .collect()
}

/// The candidate after `current` in the sweep, or `None` when the sweep is
/// exhausted (every device has been asked and none hosts a board).
///
/// Returns `Some(next)` where `next` is itself the target — `Some(None)` is
/// "try this device", `Some(Some(id))` is "try that device". A `current` that
/// has left the list (a device deregistered mid-sweep) restarts the sweep at
/// the top rather than ending it on a stale position.
pub fn next_host_candidate(
    candidates: &[Option<String>],
    current: Option<&str>,
) -> Option<Option<String>> {
    match candidates.iter().position(|c| c.as_deref() == current) {
        Some(here) => candidates.get(here + 1).cloned(),
        None => candidates.first().cloned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::board::{Filter, TaskRow};

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-08-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn row(id: &str, state: BoardState) -> TaskRow {
        TaskRow {
            id: id.into(),
            identifier: format!("gh#{id}"),
            title: format!("task {id}"),
            state: state.as_str().into(),
            source: "github".into(),
            url: format!("https://github.com/o/r/issues/{id}"),
            labels: vec![],
            dispatchable: true,
            gone: false,
            route: Some("offhand".into()),
            workspace: Some("offhand".into()),
            runtime: Some("claude-code".into()),
            chat_id: None,
            review_chat_id: None,
            pr_url: None,
            pr_number: None,
            pr_base_ref: None,
            pr_mergeable: None,
            changes_below: None,
            landing: None,
            stack: None,
            branch: Some("board/gh-x".into()),
            dispatched_by: None,
            dispatched_by_chat: None,
            last_outcome: None,
            last_outcome_at: None,
            attempts: 0,
            reopened: 0,
            updated_at: "2026-08-01T11:00:00Z".into(),
            started_at: None,
            account: None,
            dispatched_by_user: None,
            dispatched_by_verified: false,
            billed_to: None,
            max_duration_secs: None,
            context: None,
        }
    }

    #[test]
    fn sections_are_in_fixed_order_and_omit_empties() {
        let rows = vec![
            row("1", BoardState::Ready),
            row("2", BoardState::Working),
            row("3", BoardState::Done),
            row("4", BoardState::Blocked),
        ];
        let order: Vec<BoardState> = sections(&rows, &Filter::All, now())
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        assert_eq!(
            order,
            vec![
                BoardState::Blocked,
                BoardState::Working,
                BoardState::Ready,
                BoardState::Done
            ]
        );
        // review and failed had no rows and are absent entirely.
        assert!(!order.contains(&BoardState::Review));
    }

    #[test]
    fn done_is_bounded_to_today() {
        let mut old = row("old", BoardState::Done);
        old.updated_at = "2020-01-01T00:00:00Z".into();
        let mut today = row("today", BoardState::Done);
        today.updated_at = "2026-08-01T09:00:00Z".into();
        let rows = vec![old, today, row("r", BoardState::Ready)];
        let done: Vec<&str> = sections(&rows, &Filter::All, now())
            .into_iter()
            .find(|(s, _)| *s == BoardState::Done)
            .map(|(_, rows)| rows.into_iter().map(|r| r.id.as_str()).collect())
            .unwrap_or_default();
        assert_eq!(done, vec!["today"]);
    }

    #[test]
    fn a_done_section_with_nothing_from_today_is_omitted() {
        let mut old = row("old", BoardState::Done);
        old.updated_at = "2020-01-01T00:00:00Z".into();
        let rows = vec![old, row("r", BoardState::Ready)];
        assert!(
            !sections(&rows, &Filter::All, now())
                .iter()
                .any(|(s, _)| *s == BoardState::Done),
            "an all-history done section must not render at all"
        );
    }

    #[test]
    fn f_cycles_the_routes_on_the_board_and_then_back_to_all() {
        let mut rows = vec![
            row("a", BoardState::Ready),
            row("b", BoardState::Working),
            row("c", BoardState::Ready),
            row("d", BoardState::Blocked),
        ];
        rows[1].route = Some("itsm-agent".into());
        rows[2].route = Some("itsm-agent".into());
        rows[3].route = Some("tally".into());
        assert_eq!(
            routes_present(&rows, now()),
            vec!["itsm-agent", "offhand", "tally"]
        );
        assert_eq!(
            filter_cycle(&rows, now()),
            vec![
                Filter::Route("itsm-agent".into()),
                Filter::Route("offhand".into()),
                Filter::Route("tally".into()),
            ]
        );
    }

    #[test]
    fn no_route_is_a_position_in_the_cycle_after_the_named_routes() {
        let mut rows = vec![row("a", BoardState::Ready), row("u", BoardState::Ready)];
        rows[1].route = None;
        let cycle = filter_cycle(&rows, now());
        assert_eq!(
            cycle,
            vec![Filter::Route("offhand".into()), Filter::NoRoute]
        );
        assert!(
            Filter::NoRoute.matches(&rows[1]),
            "the unrouted row is the position's contents"
        );
        assert!(!Filter::NoRoute.matches(&rows[0]));
    }

    #[test]
    fn no_route_is_skipped_when_everything_routes() {
        let rows = vec![row("a", BoardState::Ready)];
        assert_eq!(
            filter_cycle(&rows, now()),
            vec![Filter::Route("offhand".into())]
        );
    }

    #[test]
    fn the_filter_is_a_view_and_a_hidden_row_stays_hidden() {
        let mut rows = vec![row("a", BoardState::Ready), row("b", BoardState::Working)];
        rows[1].route = Some("itsm-agent".into());
        let shown: Vec<&str> = sections(&rows, &Filter::Route("itsm-agent".into()), now())
            .into_iter()
            .flat_map(|(_, rows)| rows)
            .map(|r| r.id.as_str())
            .collect();
        assert_eq!(shown, vec!["b"]);
        // Nothing about the rows themselves changed — the filter is a view.
        assert_eq!(rows[1].route.as_deref(), Some("itsm-agent"));
    }

    #[test]
    fn a_typed_query_matches_identifier_title_route_and_no_route() {
        let mut rows = [row("a", BoardState::Ready), row("b", BoardState::Ready)];
        rows[1].route = None;
        assert!(Filter::Text("gh#a".into()).matches(&rows[0]));
        assert!(Filter::Text("task b".into()).matches(&rows[1]));
        assert!(Filter::Text("offhand".into()).matches(&rows[0]));
        assert!(
            Filter::Text("no route".into()).matches(&rows[1]),
            "`/no route` must reach the group"
        );
        assert!(!Filter::Text("zzz".into()).matches(&rows[0]));
        // An empty query is the whole board, not nothing.
        assert!(Filter::Text("".into()).matches(&rows[0]));
    }

    #[test]
    fn filters_say_what_they_are_and_what_to_do_when_empty() {
        assert_eq!(Filter::All.label(), None);
        assert_eq!(Filter::Route("x".into()).label(), Some("filter: x".into()));
        assert_eq!(Filter::NoRoute.label(), Some("filter: no route".into()));
        assert_eq!(Filter::Text("hi".into()).label(), Some("/hi".into()));
        assert_eq!(
            Filter::Route("x".into()).empty_note(),
            Some("Nothing on the board routes to x.  f moves on, F clears the filter.".into())
        );
        assert_eq!(Filter::All.empty_note(), None);
        assert_eq!(Filter::Text("".into()).empty_note(), None);
    }

    #[test]
    fn ready_metadata_keeps_only_what_the_grouping_does_not_already_say() {
        // The route is the group header and the repo the leading token
        // (gh#125): a workspace matching the route's name adds nothing.
        let rows = [row("a", BoardState::Ready)];
        assert_eq!(row_metadata(&rows[0], false, 80, now()), "");
        // Selected: the one action the cursor can take.
        assert!(row_metadata(&rows[0], true, 80, now()).contains("[enter to dispatch]"));

        // A workspace the route's name does not say survives on the sub-line.
        let mut renamed = row("b", BoardState::Ready);
        renamed.route = Some("tally".into());
        assert_eq!(row_metadata(&renamed, false, 80, now()), "offhand");

        // Nothing routes: the same words the filter uses.
        let mut unrouted = row("u", BoardState::Ready);
        unrouted.route = None;
        unrouted.workspace = None;
        unrouted.dispatchable = false;
        assert_eq!(row_metadata(&unrouted, false, 80, now()), "no route");
    }

    /// gh#364. The slug is decoration on the key: derived from the title, and
    /// absent rather than invented when the title has nothing in it.
    #[test]
    fn a_row_carries_a_slug_of_its_title_beside_the_identifier() {
        let mut r = row("341", BoardState::Working);
        r.title = "The review page loads nothing".into();
        assert_eq!(r.slug().as_deref(), Some("review-page-loads"));

        // Nothing but function words, and nothing at all: no slug, and the
        // identifier is still the row's name.
        r.title = "It is what it is".into();
        assert_eq!(r.slug(), None);
        r.title = String::new();
        assert_eq!(r.slug(), None);
        assert_eq!(r.display_identifier(), "gh#341");
    }

    /// The four-in-flight case this exists for: the rows the Active list draws
    /// carry the slug, and [`AgentRow::name`] is what a one-line surface says.
    #[test]
    fn live_agent_rows_are_told_apart_by_their_slugs() {
        let titles = [
            (341, "The review page loads nothing"),
            (342, "A settle announced twice is one settle"),
            (343, "It is what it is"),
        ];
        let rows: Vec<TaskRow> = titles
            .iter()
            .map(|(n, title)| {
                let mut r = row(&n.to_string(), BoardState::Working);
                r.title = (*title).into();
                r.chat_id = Some(format!("chat-{n}"));
                r
            })
            .collect();
        let chats: Vec<crate::Chat> = titles
            .iter()
            .map(|(n, _)| chat(&format!("chat-{n}"), None))
            .collect();
        let names: Vec<String> = agent_rows(&rows, &chats, &[], now())
            .iter()
            .map(AgentRow::name)
            .collect();
        assert_eq!(
            names,
            [
                "gh#341 review-page-loads",
                "gh#342 settle-announced-twice",
                // No slug in that title — the identifier alone, which is the
                // guarantee the whole feature rests on.
                "gh#343",
            ]
        );
    }

    #[test]
    fn the_leading_token_is_the_repo_qualified_form_humanized() {
        let mut r = row("507", BoardState::Ready);
        r.id = "gh:Florin-AS/tally#507".into();
        r.identifier = "gh#507".into();
        assert_eq!(r.display_identifier(), "tally #507");
        // The pull-request form of the id parses the same way and keeps its
        // own separator: `gh!508` is that row's name (gh#357), and `tally #508`
        // would be a second one — already spoken for by issue #508.
        r.id = "gh:Florin-AS/tally!508".into();
        assert_eq!(r.display_identifier(), "tally !508");
        // A Linear identifier is already unique across the board.
        r.id = "linear:LIN-142".into();
        r.identifier = "LIN-142".into();
        assert_eq!(r.display_identifier(), "LIN-142");
        // An id this rule cannot parse degrades to the identifier, never to
        // half a lockup.
        r.id = "gh:mangled".into();
        r.identifier = "gh#9".into();
        assert_eq!(r.display_identifier(), "gh#9");
        // What is rendered is what `/` matches.
        r.id = "gh:Florin-AS/tally#507".into();
        assert!(Filter::Text("tally #5".into()).matches(&r));
    }

    #[test]
    fn groups_rank_by_size_with_no_route_last_and_folded() {
        let mut rows = vec![
            row("t1", BoardState::Ready),
            row("t2", BoardState::Ready),
            row("u", BoardState::Ready),
            row("h1", BoardState::Ready),
        ];
        rows[0].route = Some("tally".into());
        rows[1].route = Some("tally".into());
        rows[2].route = None;
        rows[2].dispatchable = false;
        rows[3].route = Some("herdr-board".into());
        let grouped = grouped_sections(&rows, &Filter::All, now());
        assert_eq!(grouped.len(), 1);
        let (state, groups) = &grouped[0];
        assert_eq!(*state, BoardState::Ready);
        let labels: Vec<&str> = groups.iter().map(|g| g.label()).collect();
        // Biggest first, `no route` last regardless of size — never pole
        // position.
        assert_eq!(labels, vec!["tally", "herdr-board", "no route"]);
        assert!(
            group_starts_collapsed(&Filter::All, None),
            "unrouted rows start folded"
        );
        assert!(!group_starts_collapsed(&Filter::All, Some("tally")));
        assert!(group_headers_shown(&Filter::All, groups));
    }

    #[test]
    fn a_filter_unfolds_what_it_asked_for() {
        // `f` to the `no route` position: the rows must show, not sit behind
        // the very fold the filter was meant to reach past.
        assert!(!group_starts_collapsed(&Filter::NoRoute, None));
        // `/` matching an unrouted title: a search that hides its own match
        // reads as no match at all.
        assert!(!group_starts_collapsed(
            &Filter::Text("signicat".into()),
            None
        ));
        // And the lone group under that filter needs no header repeating what
        // the filter chip already says.
        let mut unrouted = vec![row("u", BoardState::Ready)];
        unrouted[0].route = None;
        let grouped = grouped_sections(&unrouted, &Filter::NoRoute, now());
        assert!(!group_headers_shown(&Filter::NoRoute, &grouped[0].1));
    }

    #[test]
    fn equal_sized_groups_hold_alphabetical_order_between_frames() {
        let mut rows = vec![row("b1", BoardState::Ready), row("a1", BoardState::Ready)];
        rows[0].route = Some("zeta".into());
        rows[1].route = Some("alpha".into());
        let grouped = grouped_sections(&rows, &Filter::All, now());
        let labels: Vec<&str> = grouped[0].1.iter().map(|g| g.label()).collect();
        assert_eq!(labels, vec!["alpha", "zeta"]);
    }

    #[test]
    fn a_single_routed_group_draws_no_header_but_a_lone_no_route_group_does() {
        let rows = vec![row("a", BoardState::Working)];
        let grouped = grouped_sections(&rows, &Filter::All, now());
        assert!(
            !group_headers_shown(&Filter::All, &grouped[0].1),
            "one routed group is readable bare"
        );

        let mut unrouted = vec![row("u", BoardState::Ready)];
        unrouted[0].route = None;
        let grouped = grouped_sections(&unrouted, &Filter::All, now());
        assert!(
            group_headers_shown(&Filter::All, &grouped[0].1),
            "the header is what keeps unrouted rows folded"
        );
    }

    #[test]
    fn dispatch_evidence_is_any_attempt_on_record_not_a_live_chat() {
        let mut rows = vec![row("a", BoardState::Ready), row("b", BoardState::Done)];
        assert!(
            !board_dispatched(&rows),
            "a board that only collected rows is furniture"
        );
        // The box between dispatches: nothing live, history on record.
        rows[1].attempts = 3;
        assert!(board_dispatched(&rows));
        assert!(
            !board_dispatched(&[]),
            "an empty board proves nothing either way"
        );
    }

    /// Where the work is, said after what the row needs — the pull request is
    /// the location, and the row is already named by its identifier (gh#357).
    #[test]
    fn review_metadata_says_where_the_work_is_after_what_it_needs() {
        let mut r = row("r", BoardState::Review);
        r.pr_number = Some(7);
        assert_eq!(
            row_metadata(&r, false, 80, now()),
            "waiting on you · in PR #7"
        );
        r.pr_number = None;
        assert_eq!(row_metadata(&r, false, 80, now()), "no PR · on board/gh-x");
        r.branch = None;
        assert_eq!(row_metadata(&r, false, 80, now()), "waiting on you");
    }

    /// A pull request the board never dispatched (gh#344) has no ticket, so the
    /// pull request is the only name it has — and the rule holds without an
    /// exception: the row is named, and nothing locates it a second time.
    #[test]
    fn a_pull_request_row_is_named_by_its_pull_request_and_located_once() {
        let mut r = row("508", BoardState::Review);
        r.id = "gh:Florin-AS/tally!508".into();
        r.identifier = "gh!508".into();
        r.pr_number = Some(508);
        assert!(r.is_pull_request());
        assert_eq!(r.display_identifier(), "tally !508");
        assert_eq!(row_metadata(&r, false, 80, now()), "waiting on you");
        assert_eq!(merge_confirmation(&r), "merge gh!508 into its base");

        // An issue's row is a different thing that happens to have a pull
        // request, and says where it is.
        r.id = "gh:Florin-AS/tally#508".into();
        r.identifier = "gh#508".into();
        assert!(!r.is_pull_request());
        assert_eq!(
            row_metadata(&r, false, 80, now()),
            "waiting on you · in PR #508"
        );
        assert_eq!(
            merge_confirmation(&r),
            "merge gh#508 (PR #508) into its base"
        );
    }

    /// A layer of a stack, as it comes off the wire.
    fn stacked(id: &str, position: i64, size: i64, base: &str) -> TaskRow {
        let mut r = row(id, BoardState::Review);
        r.pr_number = Some(10 + position);
        r.pr_base_ref = Some(base.into());
        r.pr_mergeable = Some("clean".into());
        r.stack = Some(RowStack {
            number: 7,
            position: Some(position),
            size: Some(size),
            base_ref: Some("main".into()),
            layers: (1..=size)
                .map(|p| StackLayer {
                    id: format!("l{p}"),
                    identifier: format!("gh!{}", 10 + p),
                    pr_number: Some(10 + p),
                    position: Some(p),
                    open: true,
                    mergeable: Some("clean".into()),
                    changes_requested: false,
                })
                .collect(),
        });
        r.id = format!("l{position}");
        r
    }

    /// The row a reader sees: which layer, and what merging it would actually
    /// do — never GitHub's `clean` on its own (gh#283).
    #[test]
    fn a_stacked_review_row_says_which_layer_and_whether_it_can_land() {
        let r = stacked("s", 2, 3, "board/gh-11-lexer");
        assert_eq!(
            row_metadata(&r, false, 80, now()),
            "2 of 3 · ready to land with 1 below · in PR #12",
        );
    }

    /// The lie this issue is about: `clean` against the layer below is not
    /// "ready to land", and the row says which branch it is clean against.
    #[test]
    fn a_clean_layer_over_a_stuck_one_says_what_clean_meant() {
        let mut r = stacked("s", 2, 3, "board/gh-11-lexer");
        r.stack.as_mut().unwrap().layers[0].mergeable = Some("dirty".into());
        assert_eq!(
            row_metadata(&r, false, 80, now()),
            "2 of 3 · clean against board/gh-11-lexer · waiting on PR #11 · in PR #12",
        );
        assert_eq!(landing(&r).as_str(), Some("waiting-on-stack"));
        assert!(!landing(&r).ready());
    }

    /// Nobody has asked GitHub yet — the ordinary state of a freshly-seen row,
    /// since mergeability rides the full sweep. The row falls back to the call
    /// to action it has always had rather than inventing a verdict.
    #[test]
    fn an_unpolled_review_row_still_says_it_is_waiting_on_you() {
        let mut r = stacked("s", 2, 3, "board/gh-11-lexer");
        r.pr_mergeable = None;
        assert_eq!(
            row_metadata(&r, false, 80, now()),
            "2 of 3 · waiting on you · in PR #12"
        );
        assert_eq!(landing(&r), Landing::Unknown);
        assert_eq!(landing_note(&r), None);
    }

    /// The fact no per-pull-request field carries (gh#389): a stack merges
    /// bottom-up, and every layer's own sentence says what that means for it.
    #[test]
    fn the_order_is_said_from_wherever_in_the_chain_you_are_standing() {
        assert_eq!(
            merge_order(&stacked("s", 1, 3, "main")).as_deref(),
            Some("bottom-up: this is the bottom open layer — #12, #13 land after it"),
        );
        assert_eq!(
            merge_order(&stacked("s", 2, 3, "board/gh-11-lexer")).as_deref(),
            Some("bottom-up: #11 lands before this one, #13 after"),
        );
        assert_eq!(
            merge_order(&stacked("s", 3, 3, "board/gh-12-parser")).as_deref(),
            Some("bottom-up: #11, #12 land before this one"),
        );
        // One pull request lands, two land.
        assert_eq!(
            merge_order(&stacked("s", 1, 2, "main")).as_deref(),
            Some("bottom-up: this is the bottom open layer — #12 lands after it"),
        );
        // Not a layer of anything, so there is no order to get wrong.
        assert_eq!(merge_order(&row("r", BoardState::Review)), None);
    }

    /// A layer that has landed is history in the chain, not something still to
    /// be sequenced — the same rule [`landing`] applies, said in words.
    #[test]
    fn a_merged_layer_is_no_longer_part_of_the_order() {
        let mut r = stacked("s", 3, 3, "board/gh-12-parser");
        r.stack.as_mut().unwrap().layers[0].open = false;
        assert_eq!(
            merge_order(&r).as_deref(),
            Some("bottom-up: #12 lands before this one"),
        );
        // And with everything under it gone there is nothing left to wait for,
        // which is the retargeted child of a merged parent.
        r.stack.as_mut().unwrap().layers[1].open = false;
        assert_eq!(merge_order(&r), None);
    }

    /// The map a detail surface draws is every sibling the board can see,
    /// bottom layer first and this row included — and each entry is named `#N`
    /// by its pull request, falling back to the row's identifier for a layer
    /// whose request has not opened yet. Asserted here because the map itself
    /// is drawn per viewport (the board panel draws chips, the review screen
    /// joins with `↑`): the *order* and the *names* are the shared contract.
    #[test]
    fn the_stack_map_is_every_layer_bottom_first_and_empty_for_an_unstacked_row() {
        let r = stacked("s", 2, 3, "board/gh-11-lexer");
        let map: Vec<String> = stack_map(&r).iter().map(layer_label).collect();
        assert_eq!(
            map,
            ["#11", "#12", "#13"],
            "bottom first, this row included"
        );

        // A layer with no pull request yet is still on the map, by name.
        let mut r = stacked("s", 2, 3, "board/gh-11-lexer");
        r.stack.as_mut().unwrap().layers[2].pr_number = None;
        let map: Vec<String> = stack_map(&r).iter().map(layer_label).collect();
        assert_eq!(map, ["#11", "#12", "gh!13"]);

        // Unconditionally callable: an unstacked row draws nothing.
        assert!(stack_map(&row("r", BoardState::Review)).is_empty());
    }

    /// The wire carries the new facts as absent rather than null on a row that
    /// has none, so a standalone pull request's JSON is the shape it always was.
    #[test]
    fn an_unstacked_row_adds_nothing_to_the_wire() {
        let wire = serde_json::to_string(&row("r", BoardState::Review)).unwrap();
        for field in [
            "stack",
            "landing",
            "pr_mergeable",
            "pr_base_ref",
            "changes_below",
        ] {
            assert!(!wire.contains(field), "{field} in {wire}");
        }
        // And an old client's row still parses: every one of them defaults.
        let back: TaskRow = serde_json::from_str(&wire).unwrap();
        assert_eq!(back.stack, None);
        assert_eq!(back.landing, None);
        assert_eq!(back.changes_below, None);
    }

    /// The direction propagation runs in (gh#289): `below` is what merging takes
    /// with it, `above` is what a rewrite moves — nearest first, because that is
    /// the order a replay reaches them in.
    #[test]
    fn above_mirrors_below_from_the_other_end_of_the_chain() {
        let stack = stacked("s", 1, 3, "main").stack.unwrap();
        let numbers = |ls: &[StackLayer]| ls.iter().filter_map(|l| l.pr_number).collect::<Vec<_>>();
        assert_eq!(numbers(stack.above("l1")), vec![12, 13]);
        assert_eq!(numbers(stack.above("l2")), vec![13]);
        assert!(stack.above("l3").is_empty());
        assert!(stack.above("nobody-here").is_empty());
    }

    /// gh#289's headline, as the row words it. A layer GitHub calls `clean` is
    /// not reviewable while the stack under it needs an ordered replay, and the
    /// answer has to outrank `clean` — that is the one answer that gets somebody
    /// to press merge, or worse, to approve a diff that then moves.
    #[test]
    fn a_layer_over_a_request_for_changes_says_so_before_it_says_clean() {
        let mut r = stacked("s", 3, 3, "board/gh-12-parser");
        assert_eq!(landing(&r), Landing::Ready { below: 2 }, "clean, before");
        r.changes_below = Some(11);

        assert_eq!(landing(&r), Landing::ChangesBelow(11));
        assert_eq!(landing(&r).as_str(), Some("changes-below"));
        assert!(!landing(&r).ready());
        assert_eq!(
            landing_note(&r).as_deref(),
            Some("PR #11 below was asked to change · this rebases under it"),
        );
        // The review row carries it where the landing verdict already went…
        assert_eq!(
            row_metadata(&r, false, 120, now()),
            "3 of 3 · PR #11 below was asked to change · this rebases under it · in PR #13",
        );
        // …and so does the last screen before something irreversible.
        assert!(
            merge_confirmation(&r)
                .ends_with("PR #11 below was asked to change · this rebases under it"),
            "{}",
            merge_confirmation(&r),
        );

        // Unknown mergeability does not mask it: not having asked GitHub whether
        // this layer is clean says nothing about the branch under it.
        r.pr_mergeable = None;
        assert_eq!(landing(&r), Landing::ChangesBelow(11));
    }

    /// The row it lands on has no agent in it and no clock to run, so without a
    /// word of its own it would sit in the loudest section of the board saying
    /// nothing at all (gh#289).
    #[test]
    fn a_layer_held_up_by_the_one_below_says_why_from_the_blocked_section() {
        let mut r = stacked("s", 3, 3, "board/gh-12-parser");
        r.state = BoardState::Blocked.as_str().into();
        r.changes_below = Some(11);
        let line = row_metadata(&r, false, 120, now());
        assert!(
            line.contains("PR #11 below was asked to change"),
            "a blocked layer says what it is waiting on: {line}"
        );

        // And a working layer, which is informed rather than stopped, reads the
        // same fact beside its clock.
        r.state = BoardState::Working.as_str().into();
        r.started_at = Some("2026-08-01T11:59:30Z".into());
        let line = row_metadata(&r, false, 120, now());
        assert!(line.contains("30s"), "{line}");
        assert!(line.contains("PR #11 below was asked to change"), "{line}");

        // A blocked row with nothing below it is exactly what it was.
        r.state = BoardState::Blocked.as_str().into();
        r.changes_below = None;
        assert!(
            !row_metadata(&r, false, 120, now()).contains("below"),
            "{}",
            row_metadata(&r, false, 120, now()),
        );
    }

    #[test]
    fn done_metadata_says_gone_upstream_or_the_workspace() {
        let mut r = row("d", BoardState::Done);
        assert!(row_metadata(&r, false, 80, now()).contains("ws:offhand"));
        r.gone = true;
        assert!(row_metadata(&r, false, 80, now()).contains("gone upstream"));
    }

    #[test]
    fn working_metadata_names_the_runtime_workspace_and_elapsed() {
        let mut r = row("w", BoardState::Working);
        r.started_at = Some("2026-08-01T11:59:30Z".into());
        let wide = row_metadata(&r, false, 120, now());
        assert!(wide.contains("claude-code"), "runtime: {wide}");
        assert!(wide.contains("ws:offhand"), "workspace: {wide}");
        assert!(wide.contains("30s"), "elapsed: {wide}");
        // Below the narrow limit everything goes rather than wrapping.
        assert_eq!(row_metadata(&r, false, 59, now()), "");
    }

    /// gh#271: two rows an hour into their work read identically on the clock,
    /// and one of them is about to have its context compacted away. The note
    /// says so — and stays out of the way while there is nothing to say.
    #[test]
    fn a_working_row_says_how_full_its_context_is_only_once_that_means_something() {
        let mut r = row("w", BoardState::Working);
        r.started_at = Some("2026-08-01T11:59:30Z".into());
        let line = |r: &TaskRow| row_metadata_line(r, false, now());

        // Nothing reported (opencode, an older CLI, a row from before this):
        // the row is exactly what it was.
        assert_eq!(line(&r), "claude-code · ws:offhand · 30s");

        // Reported, with room to spare: still quiet. A gauge that reads `ctx
        // 8%` on every row teaches the eye to skip where the warning lands.
        r.context = Some(crate::ContextUsage {
            used_tokens: 24_000,
            max_tokens: 200_000,
            compact_at_tokens: Some(167_000),
        });
        assert_eq!(line(&r), "claude-code · ws:offhand · 30s");

        // Half full: worth saying, plainly.
        r.context = Some(crate::ContextUsage {
            used_tokens: 124_000,
            max_tokens: 200_000,
            compact_at_tokens: Some(167_000),
        });
        assert_eq!(line(&r), "claude-code · ws:offhand · 30s · ctx 62%");

        // Past the point the harness itself compacts at — short of any 90%
        // rule, and the harness's own number is the one that counts.
        r.context = Some(crate::ContextUsage {
            used_tokens: 170_000,
            max_tokens: 200_000,
            compact_at_tokens: Some(167_000),
        });
        assert_eq!(
            line(&r),
            "claude-code · ws:offhand · 30s · ctx 85% compacting"
        );

        // A harness that states a window but no compaction point (codex) is
        // judged on the ratio instead.
        r.context = Some(crate::ContextUsage {
            used_tokens: 250_000,
            max_tokens: 272_000,
            compact_at_tokens: None,
        });
        assert!(line(&r).ends_with("ctx 92% compacting"), "{}", line(&r));

        // A window nobody reported is never a percentage of nothing.
        assert_eq!(
            context_note(Some(crate::ContextUsage {
                used_tokens: 90_000,
                max_tokens: 0,
                compact_at_tokens: None,
            })),
            None
        );
        assert_eq!(context_note(None), None);
    }

    /// gh#176: the desktop sets this block in a proportional font, where the
    /// terminal's padding is a ragged gap rather than a column. Same facts,
    /// joined the way the board joins facts — and the grid keeps its padding
    /// for the surface that has a grid.
    #[test]
    fn the_proportional_line_is_the_grid_without_the_padding() {
        let mut r = row("w", BoardState::Working);
        r.started_at = Some("2026-08-01T11:59:30Z".into());
        assert_eq!(
            row_metadata_line(&r, false, now()),
            "claude-code · ws:offhand · 30s"
        );
        let mut short = r.clone();
        short.runtime = Some("codex".into());
        assert_eq!(
            row_metadata(&short, false, 120, now()),
            "codex       ws:offhand 30s",
            "the terminal keeps its cells"
        );
        assert_eq!(
            row_metadata_line(&short, false, now()),
            "codex · ws:offhand · 30s"
        );
        // Every fact the grid carries survives the join, and nothing empty
        // arrives as a stray separator.
        let mut done = row("d", BoardState::Done);
        done.runtime = None;
        assert_eq!(row_metadata_line(&done, false, now()), "ws:offhand");
        // A ready row with nothing to say says nothing — the whole reason the
        // second line could go.
        assert_eq!(
            row_metadata_line(&row("r", BoardState::Ready), false, now()),
            ""
        );
        // The billing note is still last, and still separated.
        let mut billed = row("b", BoardState::Working);
        billed.started_at = Some("2026-08-01T11:59:30Z".into());
        billed.billed_to = Some("someone@else.example".into());
        billed.dispatched_by_user = Some("brede@tally.no".into());
        let line = row_metadata_line(&billed, false, now());
        assert!(
            line.ends_with(&bills_label("someone@else.example")),
            "{line}"
        );
    }

    #[test]
    fn elapsed_never_goes_negative() {
        let mut r = row("w", BoardState::Working);
        r.started_at = Some("2026-08-01T12:01:00Z".into());
        assert!(row_metadata(&r, false, 80, now()).contains("0s"));
    }

    #[test]
    fn unknown_wire_state_reads_as_ready_not_a_crash() {
        let mut r = row("x", BoardState::Ready);
        r.state = "bogus".into();
        assert_eq!(r.state(), BoardState::Ready);
    }

    #[test]
    fn task_row_round_trips_through_json() {
        let r = row("a", BoardState::Review);
        let json = serde_json::to_string(&r).unwrap();
        let back: TaskRow = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
        // A pre-timestamp row still deserializes.
        let mut bare = serde_json::to_value(&r).unwrap();
        bare.as_object_mut().unwrap().remove("updated_at");
        bare.as_object_mut().unwrap().remove("started_at");
        let back: TaskRow = serde_json::from_value(bare).unwrap();
        assert_eq!(back.updated_at, "");
        assert_eq!(back.started_at, None);
    }

    #[test]
    fn format_helpers_read_as_people_mean_them() {
        assert_eq!(format_elapsed(12), "12s");
        assert_eq!(format_elapsed(9 * 60 + 4), "9m04s");
        assert_eq!(format_elapsed(3600 + 1200), "1h20m");
        assert_eq!(format_age(12), "12s");
        assert_eq!(format_age(240), "4m");
        assert_eq!(format_age(3 * 3600), "3h");
        // A cap is a round number somebody typed, and reads as one.
        assert_eq!(format_cap(7200), "2h");
        assert_eq!(format_cap(45 * 60), "45m");
        assert_eq!(format_cap(90 * 60), "1h30m");
        assert_eq!(format_cap(30), "30s");
    }

    // ---- live agents (gh#103) ---------------------------------------------

    fn chat(id: &str, branch: Option<&str>) -> crate::Chat {
        crate::Chat {
            id: id.into(),
            device_id: "box".into(),
            title: Some("some title the agent wrote".into()),
            archived: false,
            cwd: Some(format!("/w/{id}")),
            branch: branch.map(str::to_string),
            checkout_id: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: now(),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: Some("space".into()),
            last_seen_at: None,
            forked_from: None,
        }
    }

    fn session(chat_id: &str, status: crate::SessionStatus, age_ms: i64) -> crate::Session {
        crate::Session {
            chat_id: chat_id.into(),
            device_id: "box".into(),
            status,
            started_at: None,
            updated_at: now() - chrono::Duration::milliseconds(age_ms),
        }
    }

    /// A live attempt is a `working`/`blocked` row with a chat, and the chat has
    /// to be one this viewport can actually open.
    #[test]
    fn only_live_attempts_with_a_reachable_chat_are_agents() {
        let mut live = row("live", BoardState::Working);
        live.chat_id = Some("chat-live".into());
        // Dispatched, but its chat has not synced here (or was never shared).
        let mut orphan = row("orphan", BoardState::Working);
        orphan.chat_id = Some("chat-elsewhere".into());
        // Finished: the attempt closed, so the row kept neither state nor chat.
        let settled = row("settled", BoardState::Review);
        let ready = row("ready", BoardState::Ready);

        let rows = vec![live, orphan, settled, ready];
        let agents = agent_rows(&rows, &[chat("chat-live", None)], &[], now());
        assert_eq!(
            agents
                .iter()
                .map(|a| a.task_id.as_str())
                .collect::<Vec<_>>(),
            vec!["live"]
        );
    }

    /// The state on screen is the session watch's, not the board's — the board's
    /// is a sync cycle old, and the split of its one `blocked` into a question
    /// and a corpse is the whole reason the section exists.
    #[test]
    fn state_comes_from_the_session_watch_and_falls_back_to_the_row() {
        let mut asking = row("ask", BoardState::Working);
        asking.chat_id = Some("c1".into());
        let mut died = row("died", BoardState::Working);
        died.chat_id = Some("c2".into());
        let mut no_session = row("fresh", BoardState::Blocked);
        no_session.chat_id = Some("c3".into());
        let mut stale = row("stale", BoardState::Working);
        stale.chat_id = Some("c4".into());

        let chats = vec![
            chat("c1", None),
            chat("c2", None),
            chat("c3", None),
            chat("c4", None),
        ];
        let sessions = vec![
            session("c1", crate::SessionStatus::AwaitingInput, 0),
            session("c2", crate::SessionStatus::Errored, 0),
            // Older than the staleness window: a crashed backend must not leave
            // an eternal spinner in the sidebar.
            session(
                "c4",
                crate::SessionStatus::Working,
                crate::view::SESSION_STALE_MS + 1_000,
            ),
        ];
        let agents = agent_rows(&[asking, died, no_session, stale], &chats, &sessions, now());
        let state = |id: &str| {
            agents
                .iter()
                .find(|a| a.task_id == id)
                .map(|a| a.state)
                .unwrap()
        };
        assert_eq!(state("ask"), AgentState::Blocked);
        assert_eq!(state("died"), AgentState::Errored);
        // No session row yet: the board's verdict stands in.
        assert_eq!(state("fresh"), AgentState::Blocked);
        assert_eq!(state("stale"), AgentState::Working);
    }

    /// Blocked floats, and the badge counts everything that wants a human.
    #[test]
    fn blocked_floats_to_the_top_and_the_badge_counts_them() {
        let mut fast = row("fast", BoardState::Working);
        fast.chat_id = Some("c1".into());
        fast.started_at = Some("2026-08-01T11:59:00Z".into()); // 1m
        let mut slow = row("slow", BoardState::Working);
        slow.chat_id = Some("c2".into());
        slow.started_at = Some("2026-08-01T10:00:00Z".into()); // 2h
        let mut asking = row("ask", BoardState::Blocked);
        asking.chat_id = Some("c3".into());
        asking.started_at = Some("2026-08-01T11:55:00Z".into()); // 5m
        let mut died = row("died", BoardState::Working);
        died.chat_id = Some("c4".into());

        let chats = vec![
            chat("c1", None),
            chat("c2", None),
            chat("c3", None),
            chat("c4", None),
        ];
        let sessions = vec![
            session("c1", crate::SessionStatus::Working, 0),
            session("c2", crate::SessionStatus::Working, 0),
            session("c3", crate::SessionStatus::AwaitingInput, 0),
            session("c4", crate::SessionStatus::Errored, 0),
        ];
        let agents = agent_rows(&[fast, slow, asking, died], &chats, &sessions, now());
        assert_eq!(
            agents
                .iter()
                .map(|a| a.task_id.as_str())
                .collect::<Vec<_>>(),
            // A question, then a dead run, then the longest-running worker.
            vec!["ask", "died", "slow", "fast"]
        );
        let active: Vec<ActiveRow> = agents.into_iter().map(ActiveRow::Agent).collect();
        assert_eq!(active_needing_attention(&active), 2);
    }

    /// "That one's been at it 1h50 of 2h" — a glance, which needs both numbers.
    #[test]
    fn elapsed_reads_against_the_routes_cap() {
        let mut r = row("x", BoardState::Working);
        r.chat_id = Some("c1".into());
        r.started_at = Some("2026-08-01T10:10:00Z".into()); // 1h50m
        r.max_duration_secs = Some(7200);
        let agents = agent_rows(&[r.clone()], &[chat("c1", None)], &[], now());
        assert_eq!(
            agents[0].elapsed_label(now()).as_deref(),
            Some("1h50m / 2h")
        );
        assert!(!agents[0].over_cap(now()));

        // Past the cap: gh#70's clock is about to end it, and the row says so.
        let mut over = r.clone();
        over.started_at = Some("2026-08-01T09:00:00Z".into()); // 3h
        let agents = agent_rows(&[over], &[chat("c1", None)], &[], now());
        assert_eq!(
            agents[0].elapsed_label(now()).as_deref(),
            Some("3h00m / 2h")
        );
        assert!(agents[0].over_cap(now()));

        // An uncapped route says only how long it has been.
        let mut uncapped = r.clone();
        uncapped.max_duration_secs = None;
        let agents = agent_rows(&[uncapped], &[chat("c1", None)], &[], now());
        assert_eq!(agents[0].elapsed_label(now()).as_deref(), Some("1h50m"));
        assert!(!agents[0].over_cap(now()));

        // A board with no `started_at` (predates the field) says nothing rather
        // than counting up from the epoch.
        let mut undated = r;
        undated.started_at = None;
        let agents = agent_rows(&[undated], &[chat("c1", None)], &[], now());
        assert_eq!(agents[0].elapsed_label(now()), None);
        assert!(!agents[0].over_cap(now()));
    }

    /// The sub-line is the checkout the agent is in — the chat's own branch
    /// first, the attempt's as recorded when the chat has none.
    #[test]
    fn the_sub_line_prefers_the_chats_branch() {
        let mut r = row("x", BoardState::Working);
        r.chat_id = Some("c1".into());
        r.branch = Some("board/gh-103".into());
        let renamed = [chat("c1", Some("board/gh-103-renamed"))];
        let agents = agent_rows(&[r.clone()], &renamed, &[], now());
        assert_eq!(agents[0].branch.as_deref(), Some("board/gh-103-renamed"));
        let agents = agent_rows(&[r], &[chat("c1", None)], &[], now());
        assert_eq!(agents[0].branch.as_deref(), Some("board/gh-103"));
    }

    // ---- running, non-board chats (gh#117) --------------------------------

    fn started(chat_id: &str, status: crate::SessionStatus, started: &str) -> crate::Session {
        crate::Session {
            started_at: Some(
                DateTime::parse_from_rfc3339(started)
                    .unwrap()
                    .with_timezone(&Utc),
            ),
            ..session(chat_id, status, 0)
        }
    }

    /// The whole point: a chat working with no attempt behind it is presence
    /// the board cannot report, and it shows anyway.
    #[test]
    fn a_working_chat_with_no_attempt_is_a_running_row() {
        let chats = vec![
            chat("orchestrator", None),
            chat("adhoc", None),
            chat("idle", None),
        ];
        let sessions = vec![
            started(
                "orchestrator",
                crate::SessionStatus::Working,
                "2026-08-01T11:30:00Z",
            ),
            started(
                "adhoc",
                crate::SessionStatus::AwaitingInput,
                "2026-08-01T11:58:00Z",
            ),
        ];
        let running = running_rows(&[], &chats, &sessions, None, now());
        assert_eq!(
            running
                .iter()
                .map(|r| r.chat_id.as_str())
                .collect::<Vec<_>>(),
            // Blocked floats; the idle chat is not a run at all.
            vec!["adhoc", "orchestrator"]
        );
        assert_eq!(running[0].state, AgentState::Blocked);
        assert_eq!(running[1].elapsed_label(now()).as_deref(), Some("30m00s"));
    }

    /// Every membership edge in one place: stale is dead, errored is not a run,
    /// and archiving a chat that is working anyway does not hide it.
    #[test]
    fn membership_is_the_live_session_watch_and_nothing_else() {
        let mut archived = chat("archived", None);
        archived.archived = true;
        let chats = vec![
            chat("stale", None),
            chat("errored", None),
            chat("never", None),
            archived,
        ];
        let sessions = vec![
            // A crashed backend must not leave an eternal row here either.
            session(
                "stale",
                crate::SessionStatus::Working,
                crate::view::SESSION_STALE_MS + 1_000,
            ),
            session("errored", crate::SessionStatus::Errored, 0),
            session("archived", crate::SessionStatus::Working, 0),
        ];
        let running = running_rows(&[], &chats, &sessions, None, now());
        assert_eq!(
            running
                .iter()
                .map(|r| r.chat_id.as_str())
                .collect::<Vec<_>>(),
            vec!["archived"],
            "archiving is a decision about a finished chat, not about a live run"
        );
    }

    /// The two groups partition the box's load: a dispatched chat is the Agents
    /// group's, and counting it twice would misreport what is running.
    #[test]
    fn a_live_attempts_chat_belongs_to_agents_and_not_to_running() {
        let mut live = row("live", BoardState::Working);
        live.chat_id = Some("c-live".into());
        // A settled attempt released its chat — if the operator kept talking in
        // it, that IS an unmanaged run and belongs here.
        let mut settled = row("settled", BoardState::Review);
        settled.chat_id = Some("c-settled".into());

        let chats = vec![chat("c-live", None), chat("c-settled", None)];
        let sessions = vec![
            session("c-live", crate::SessionStatus::Working, 0),
            session("c-settled", crate::SessionStatus::Working, 0),
        ];
        let rows = vec![live, settled];
        assert_eq!(
            running_rows(&rows, &chats, &sessions, None, now())
                .iter()
                .map(|r| r.chat_id.as_str())
                .collect::<Vec<_>>(),
            vec!["c-settled"]
        );
        // And it is in exactly one of the two groups, not neither.
        assert_eq!(
            agent_rows(&rows, &chats, &sessions, now())
                .iter()
                .map(|a| a.chat_id.as_str())
                .collect::<Vec<_>>(),
            vec!["c-live"]
        );
    }

    /// A chat claimed by an attempt stays out even when the Agents group drops
    /// it — the subtraction reads the board rows, not the other list.
    #[test]
    fn a_dispatched_chat_the_agents_group_dropped_is_still_not_running() {
        let mut live = row("live", BoardState::Blocked);
        live.chat_id = Some("c1".into());
        let chats = vec![chat("c1", None)];
        let sessions = vec![session("c1", crate::SessionStatus::Working, 0)];
        // agent_rows keeps it (the chat synced) — but the subtraction must not
        // depend on that having happened.
        assert!(!agent_rows(std::slice::from_ref(&live), &chats, &sessions, now()).is_empty());
        assert!(running_rows(&[live], &chats, &sessions, None, now()).is_empty());
    }

    /// What the row can say: the chat's own title, the run's age, a badge.
    #[test]
    fn a_running_row_says_the_title_the_age_and_who_wants_a_human() {
        let untitled = crate::Chat {
            title: Some("   ".into()),
            ..chat("blank", None)
        };
        let chats = vec![chat("named", None), untitled];
        let sessions = vec![
            started(
                "named",
                crate::SessionStatus::AwaitingInput,
                "2026-08-01T11:00:00Z",
            ),
            // No `started_at`: the row draws without a counter rather than
            // counting up from the epoch.
            session("blank", crate::SessionStatus::Working, 0),
        ];
        let running = running_rows(&[], &chats, &sessions, None, now());
        assert_eq!(running[0].title, "some title the agent wrote");
        assert_eq!(running[0].elapsed_label(now()).as_deref(), Some("1h00m"));
        assert_eq!(running[1].title, UNTITLED_CHAT);
        assert_eq!(running[1].elapsed_label(now()), None);
        let active: Vec<ActiveRow> = running.into_iter().map(ActiveRow::Unmanaged).collect();
        assert_eq!(active_needing_attention(&active), 1);
    }

    /// A box with no board is the case this group matters most on — nothing to
    /// subtract, and the whole live list shows.
    #[test]
    fn no_board_subtracts_nothing() {
        let chats = vec![chat("c1", None)];
        let sessions = vec![session("c1", crate::SessionStatus::Working, 0)];
        assert_eq!(running_rows(&[], &chats, &sessions, None, now()).len(), 1);
    }

    /// The pinned orchestrator has a slot of its own (gh#122) — a working
    /// orchestrator is the slot's news, not a Running row.
    #[test]
    fn the_pinned_orchestrator_is_the_slots_not_runnings() {
        let chats = vec![chat("orch", None), chat("adhoc", None)];
        let sessions = vec![
            session("orch", crate::SessionStatus::Working, 0),
            session("adhoc", crate::SessionStatus::Working, 0),
        ];
        assert_eq!(
            running_rows(&[], &chats, &sessions, Some("orch"), now())
                .iter()
                .map(|r| r.chat_id.as_str())
                .collect::<Vec<_>>(),
            vec!["adhoc"]
        );
    }

    // ---- the Active group (gh#123) ----------------------------------------

    /// The order the merge exists for: a hand-started run asking a question
    /// outranks a board attempt working fine. Needs-you first, then working,
    /// and inside a rank the longest-running first — origin never consulted.
    #[test]
    fn the_active_group_orders_by_urgency_not_by_origin() {
        let mut attempt = row("live", BoardState::Working);
        attempt.chat_id = Some("c-attempt".into());
        attempt.started_at = Some("2026-08-01T11:00:00Z".into()); // 1h
        let chats = vec![
            chat("c-attempt", None),
            chat("c-adhoc", None),
            chat("c-orch", None),
        ];
        let sessions = vec![
            session("c-attempt", crate::SessionStatus::Working, 0),
            started(
                "c-adhoc",
                crate::SessionStatus::AwaitingInput,
                "2026-08-01T11:58:00Z",
            ),
            started(
                "c-orch",
                crate::SessionStatus::Working,
                "2026-08-01T09:00:00Z",
            ), // 3h
        ];
        let active = active_rows(&[attempt], &chats, &sessions, None, now());
        assert_eq!(
            active.iter().map(|r| r.chat_id()).collect::<Vec<_>>(),
            // The question first, wherever it came from; then the workers,
            // longest-running first, the attempt's origin buying it nothing.
            vec!["c-adhoc", "c-orch", "c-attempt"]
        );
        assert_eq!(active_needing_attention(&active), 1);
        // Origin still shows on the row: the attempt kept its issue, branch
        // and cap by staying the Agent variant.
        assert!(matches!(
            &active[2],
            ActiveRow::Agent(a) if a.identifier == "gh#live"
        ));
        assert!(matches!(&active[0], ActiveRow::Unmanaged(_)));
    }

    /// The merge changes the order, never the membership: a dispatched chat
    /// draws once, as the attempt that claimed it.
    #[test]
    fn a_dispatched_chat_draws_once_in_the_active_group() {
        let mut live = row("live", BoardState::Working);
        live.chat_id = Some("c1".into());
        let chats = vec![chat("c1", None)];
        let sessions = vec![session("c1", crate::SessionStatus::Working, 0)];
        let active = active_rows(&[live], &chats, &sessions, None, now());
        assert_eq!(active.len(), 1);
        assert!(matches!(&active[0], ActiveRow::Agent(_)));
    }

    // ---- whose subscription (gh#101) ------------------------------------

    fn login(
        id: &str,
        email: &str,
        harness: crate::HarnessId,
        active: bool,
    ) -> crate::AgentAccount {
        crate::AgentAccount {
            id: id.into(),
            harness,
            email: Some(email.into()),
            plan_label: None,
            active,
            usage_windows: Vec::new(),
            display_name: None,
            organization: None,
            auth_kind: None,
            switchable: true,
            saved_at: None,
        }
    }

    /// A named slot resolves to its own login; naming none resolves to the
    /// box's live one, which is what a dispatch with no `account` really
    /// spends. Both are per harness — a Claude slot cannot pay for a codex run.
    #[test]
    fn billed_email_names_the_slot_or_the_boxs_own_login() {
        use crate::HarnessId::{ClaudeCode, Codex};
        let accounts = vec![
            login("slot-box", "brede@tally.no", ClaudeCode, true),
            login("slot-ana", "ana@example.com", ClaudeCode, false),
            login("slot-cod", "cod@example.com", Codex, true),
        ];
        assert_eq!(
            billed_email(&accounts, ClaudeCode, Some("slot-ana")),
            Some("ana@example.com")
        );
        assert_eq!(
            billed_email(&accounts, ClaudeCode, None),
            Some("brede@tally.no"),
            "no slot named is the box's own login, not nothing"
        );
        assert_eq!(
            billed_email(&accounts, Codex, None),
            Some("cod@example.com")
        );
        // A slot saved for another harness is not this run's to spend.
        assert_eq!(billed_email(&accounts, Codex, Some("slot-ana")), None);
        // Nothing saved at all: the box cannot name whose it is, and says so.
        assert_eq!(billed_email(&[], ClaudeCode, None), None);
    }

    /// The guard's whole comparison — the same one whether the dispatcher's
    /// name was verified or claimed (gh#161 decides *that* elsewhere). Two
    /// unknowns must never read as an accusation: an unattributed dispatch
    /// names nobody to have wronged.
    #[test]
    fn cross_billed_is_a_dispatcher_against_a_slot_email_and_nothing_else() {
        assert!(cross_billed(
            Some("brede@tally.no"),
            Some("ana@example.com")
        ));
        assert!(!cross_billed(
            Some("brede@tally.no"),
            Some("BREDE@Tally.no")
        ));
        assert!(!cross_billed(Some("brede@tally.no"), None));
        assert!(!cross_billed(None, Some("ana@example.com")));
        assert!(!cross_billed(None, None));
        // Empty strings are absence, not a name that differs from everything.
        assert!(!cross_billed(Some("brede@tally.no"), Some("  ")));
    }

    /// A cross-billed attempt says so for its whole life, in the metadata both
    /// viewports draw — working, in review, and long closed.
    #[test]
    fn a_cross_billed_row_says_whose_subscription_it_spends() {
        for state in [BoardState::Working, BoardState::Review, BoardState::Done] {
            let mut r = row("b", state);
            r.billed_to = Some("brede@tally.no".into());
            r.dispatched_by_user = Some("ana@example.com".into());
            r.started_at = Some("2026-08-01T11:59:30Z".into());
            let meta = row_metadata(&r, false, 120, now());
            assert!(
                meta.contains("bills brede@tally.no"),
                "{state:?} row must name the payer: {meta}"
            );
        }

        // The owner releasing their own work is not cross-billed and says
        // nothing extra — the warning has to stay rare enough to mean something.
        let mut own = row("o", BoardState::Working);
        own.billed_to = Some("brede@tally.no".into());
        own.dispatched_by_user = Some("brede@tally.no".into());
        assert_eq!(billing_note(&own), None);
        assert!(!row_metadata(&own, false, 120, now()).contains("bills"));

        // And a row nothing has run on carries no verdict at all.
        assert_eq!(billing_note(&row("r", BoardState::Ready)), None);
    }

    #[test]
    fn the_billing_words_are_the_same_words_everywhere() {
        assert_eq!(bills_label("brede@tally.no"), "bills brede@tally.no");
        assert_eq!(
            bills_warning("brede@tally.no", crate::HarnessId::ClaudeCode),
            "this run bills brede@tally.no's Claude — pass --account <your slot>"
        );
        assert_eq!(
            bills_comment_suffix("brede@tally.no"),
            " · on brede@tally.no's subscription"
        );
        assert_eq!(
            subscription_noun(crate::HarnessId::Codex),
            "Codex",
            "named as its owner thinks of it, not as comet spells the runtime"
        );
    }

    fn device(id: &str, created: &str) -> crate::Device {
        crate::Device {
            id: id.into(),
            name: id.into(),
            platform: "macos".into(),
            last_seen_at: None,
            created_at: Some(
                DateTime::parse_from_rfc3339(created)
                    .unwrap()
                    .with_timezone(&Utc),
            ),
            version: None,
        }
    }

    #[test]
    fn host_sweep_asks_this_device_first_then_the_rest_in_registration_order() {
        let devices = vec![
            device("box", "2026-01-02T00:00:00Z"),
            device("laptop", "2026-01-01T00:00:00Z"),
            device("phone", "2026-01-03T00:00:00Z"),
        ];
        let candidates = host_candidates(&devices, Some("laptop"));
        assert_eq!(
            candidates,
            vec![None, Some("box".into()), Some("phone".into())],
            "this device first, then the others oldest-registered first"
        );
        // The sweep walks the list once and then stops: a board that is
        // nowhere must read as "nowhere", not loop forever.
        assert_eq!(
            next_host_candidate(&candidates, None),
            Some(Some("box".into()))
        );
        assert_eq!(
            next_host_candidate(&candidates, Some("box")),
            Some(Some("phone".into()))
        );
        assert_eq!(next_host_candidate(&candidates, Some("phone")), None);
    }

    #[test]
    fn a_device_that_left_mid_sweep_restarts_the_sweep() {
        let devices = vec![device("box", "2026-01-02T00:00:00Z")];
        let candidates = host_candidates(&devices, Some("laptop"));
        assert_eq!(next_host_candidate(&candidates, Some("gone")), Some(None));
    }

    #[test]
    fn a_lone_device_still_asks_itself() {
        assert_eq!(host_candidates(&[], None), vec![None]);
        // Even when the local id is unknown (LocalDevice hasn't answered yet),
        // every registered device is a candidate — asking the box twice under
        // two names is harmless; never asking it is not.
        let devices = vec![device("box", "2026-01-02T00:00:00Z")];
        assert_eq!(
            host_candidates(&devices, None),
            vec![None, Some("box".into())]
        );
    }

    // ---- the detail surface (gh#132) -------------------------------------

    #[test]
    fn a_ready_row_releases_and_an_unrouted_one_offers_nothing() {
        let ready = row("1", BoardState::Ready);
        assert_eq!(row_actions(&ready), vec![RowAction::Dispatch]);
        let mut stranded = row("2", BoardState::Ready);
        stranded.dispatchable = false;
        assert!(row_actions(&stranded).is_empty());
    }

    #[test]
    fn a_live_row_opens_and_cancels_and_a_blocked_one_may_also_retry() {
        let working = row("1", BoardState::Working);
        assert_eq!(
            row_actions(&working),
            vec![RowAction::OpenChat, RowAction::Cancel]
        );
        let blocked = row("2", BoardState::Blocked);
        assert_eq!(
            row_actions(&blocked),
            vec![RowAction::Retry, RowAction::OpenChat, RowAction::Cancel]
        );
    }

    /// The review door opens on anything that has run (§gh#180), which is a
    /// different question from what state the row is in — a failed attempt has
    /// the most interesting diff on the board.
    #[test]
    fn anything_that_has_run_can_be_reviewed_whatever_state_it_ended_in() {
        for state in [
            BoardState::Working,
            BoardState::Blocked,
            BoardState::Review,
            BoardState::Failed,
            BoardState::Done,
        ] {
            let mut ran = row("1", state);
            ran.attempts = 1;
            assert!(reviewable(&ran), "{state:?} has an attempt to review");
        }
        // A row nobody has dispatched and nobody has pushed has no attempt, so
        // no diff, no claims and no journal. The door would open on an empty
        // room.
        assert!(!reviewable(&row("2", BoardState::Ready)));
    }

    /// §gh#344: the pull request an agent opened from its own chat instead of
    /// dispatching. No attempt, and the most-needed reading on the board.
    #[test]
    fn a_pull_request_nobody_dispatched_still_opens_the_door() {
        let mut undispatched = row("gh:o/r#191", BoardState::Review);
        undispatched.attempts = 0;
        assert!(!reviewable(&undispatched), "nothing to review yet");
        undispatched.pr_url = Some("https://github.com/o/r/pull/191".into());
        assert!(reviewable(&undispatched), "the diff is on GitHub");
        // An empty string is what an absent URL looks like on some wires, and
        // it is not a pull request.
        undispatched.pr_url = Some(String::new());
        assert!(!reviewable(&undispatched));
    }

    #[test]
    fn a_review_row_offers_its_pr_only_when_there_is_one() {
        let mut review = row("1", BoardState::Review);
        assert!(row_actions(&review).is_empty());
        review.pr_url = Some("https://github.com/o/r/pull/9".into());
        assert_eq!(row_actions(&review), vec![RowAction::OpenPr]);
    }

    // ---- the one visible verb (gh#176) ------------------------------------

    #[test]
    fn the_primary_verb_is_the_one_enter_runs() {
        assert_eq!(
            primary_action(&row("1", BoardState::Ready)),
            Some(RowAction::Dispatch)
        );
        let mut failed = row("2", BoardState::Failed);
        failed.state = BoardState::Failed.as_str().into();
        assert_eq!(primary_action(&failed), Some(RowAction::Retry));
        assert_eq!(
            primary_action(&row("3", BoardState::Working)),
            Some(RowAction::OpenChat)
        );
        // Not the first of its actions: a blocked row leads with Retry and
        // still opens on enter, which is exactly why the designation is not
        // "whatever came first" (gh#49).
        let blocked = row("4", BoardState::Blocked);
        assert_eq!(row_actions(&blocked)[0], RowAction::Retry);
        assert_eq!(primary_action(&blocked), Some(RowAction::OpenChat));
    }

    #[test]
    fn a_row_with_nothing_to_do_wears_no_verb() {
        assert_eq!(primary_action(&row("1", BoardState::Done)), None);
        let mut stranded = row("2", BoardState::Ready);
        stranded.dispatchable = false;
        assert_eq!(primary_action(&stranded), None);
        // A review with no PR raised has nothing to open yet; one with a PR
        // wears it.
        let mut review = row("3", BoardState::Review);
        assert_eq!(primary_action(&review), None);
        review.pr_url = Some("https://github.com/o/r/pull/9".into());
        assert_eq!(primary_action(&review), Some(RowAction::OpenPr));
    }

    #[test]
    fn primary_and_secondary_are_the_whole_action_set_and_no_more() {
        for state in BoardState::SECTION_ORDER {
            let mut r = row("1", state);
            r.state = state.as_str().into();
            r.pr_url = Some("https://github.com/o/r/pull/9".into());
            let mut split = Vec::new();
            split.extend(primary_action(&r));
            split.extend(secondary_actions(&r));
            split.sort_by_key(|a| format!("{a:?}"));
            let mut all = row_actions(&r);
            all.sort_by_key(|a| format!("{a:?}"));
            assert_eq!(split, all, "{state:?}");
        }
    }

    #[test]
    fn every_action_has_a_sentence_spelling() {
        for action in [
            RowAction::Dispatch,
            RowAction::Retry,
            RowAction::Cancel,
            RowAction::OpenChat,
            RowAction::OpenIssue,
            RowAction::OpenPr,
        ] {
            let verb = action.verb();
            assert!(!verb.is_empty());
            // A verb reads inside a sentence, so it starts lower case — and
            // "PR" stays "PR".
            assert!(verb.starts_with(|c: char| c.is_lowercase()), "{verb}");
        }
        assert_eq!(RowAction::OpenPr.verb(), "open PR");
    }

    #[test]
    fn the_detail_adds_the_links_a_list_has_no_room_for() {
        let mut ready = row("1", BoardState::Ready);
        ready.pr_url = Some("https://github.com/o/r/pull/9".into());
        assert_eq!(
            detail_actions(&ready),
            vec![RowAction::Dispatch, RowAction::OpenPr, RowAction::OpenIssue]
        );
        // Never twice: a review row already offers its PR as its own action.
        let mut review = row("2", BoardState::Review);
        review.pr_url = Some("https://github.com/o/r/pull/9".into());
        assert_eq!(
            detail_actions(&review),
            vec![RowAction::OpenPr, RowAction::OpenIssue]
        );
    }

    #[test]
    fn a_done_row_still_opens_its_issue() {
        let done = row("1", BoardState::Done);
        assert!(row_actions(&done).is_empty());
        assert_eq!(detail_actions(&done), vec![RowAction::OpenIssue]);
        assert_eq!(
            action_url(&done, RowAction::OpenIssue),
            Some("https://github.com/o/r/issues/1")
        );
        assert_eq!(action_url(&done, RowAction::Dispatch), None);
    }

    #[test]
    fn a_row_nobody_has_run_has_no_history_to_show() {
        assert_eq!(history_line(&row("1", BoardState::Ready), now()), None);
    }

    #[test]
    fn the_history_line_counts_attempts_and_names_the_last_outcome() {
        let mut r = row("1", BoardState::Ready);
        r.attempts = 2;
        r.last_outcome = Some("failed".into());
        r.last_outcome_at = Some("2026-08-01T09:00:00Z".into());
        assert_eq!(
            history_line(&r, now()).unwrap(),
            "attempt 2 · last failed 3h ago"
        );
    }

    #[test]
    fn the_history_line_names_who_paid_even_when_it_was_you() {
        let mut r = row("1", BoardState::Working);
        r.attempts = 1;
        r.billed_to = Some("brede@tally.no".into());
        r.dispatched_by_user = Some("brede@tally.no".into());
        // `billing_note` stays silent — nobody else is paying — but the detail
        // answers the question it was opened to answer.
        assert_eq!(billing_note(&r), None);
        assert_eq!(
            history_line(&r, now()).unwrap(),
            "attempt 1 · bills brede@tally.no"
        );
    }

    #[test]
    fn a_reopened_attempt_says_so_where_there_is_room_for_it() {
        let mut r = row("1", BoardState::Working);
        r.attempts = 1;
        r.reopened = 2;
        assert_eq!(history_line(&r, now()).unwrap(), "attempt 1 · reopened 2×");
    }

    #[test]
    fn placement_names_the_route_the_runtime_the_space_and_the_branch() {
        let r = row("1", BoardState::Working);
        assert_eq!(
            placement_line(&r).unwrap(),
            "offhand · claude-code · ws:offhand · board/gh-x"
        );
    }

    #[test]
    fn placement_says_no_route_rather_than_leading_with_a_runtime() {
        let mut r = row("1", BoardState::Ready);
        r.route = None;
        r.workspace = None;
        r.branch = None;
        assert_eq!(placement_line(&r).unwrap(), "no route · claude-code");
    }
}
