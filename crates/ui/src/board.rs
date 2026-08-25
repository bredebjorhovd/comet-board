//! The board panel (§gh#70): the herdr task board as a citizen of
//! the desktop app — a right dock toggled with Cmd/Ctrl+Shift+B, fed by the
//! engine's `WatchBoard` stream.
//!
//! The *derivations* all live in `comet_proto::view::board` — the section
//! grouping, the `f`/`/` filter cycle, what each row says — so this panel
//! renders exactly the rows the iOS board screen renders (its Swift mirrors
//! read the same `TaskRow` wire shape), with the same vocabulary. This module holds only the interactive state the derivations
//! need a home for: the rows as they streamed, the selection, the filter, the
//! folded sections, and the dispatch/cancel/open-chat verbs.
//!
//! - Sections in fixed order (blocked → working → ready → review → failed →
//!   done), empty ones omitted; `done` is bounded to today by the shared
//!   derivation. A group header is a 26px line between two hairlines, with no
//!   fill of its own (gh#295, claim E4);
//! - a row is one 32px line in fixed columns (gh#176, gh#295, claim E6): the
//!   status glyph, the id, the title, the repo, the time, then the row's own
//!   verb as a neutral chip. Every row is that height in every state, so
//!   nothing the pointer does reflows the list (gh#132);
//! - `enter` runs the selected row's primary verb — the one the row draws
//!   without being hovered, taken from `board::primary_action` so the chip and
//!   the key are one rule (gh#176): dispatch a ready row (`DispatchTask`),
//!   retry a failed one, open a running or blocked one's chat, open a review's
//!   PR. On a section header it folds/unfolds;
//! - the dispatch picker asks which runtime, whose agent account, and which
//!   model on that harness (`ListModels`, defaulted to the harness's first
//!   catalog row) to release under, and sends all three as overrides. The
//!   account strip (`ListAgentAccounts` on the board's host, filtered to the
//!   highlighted runtime's harness — gh#74) leads with the route's own account,
//!   which sends no override; picking a slot is how a teammate spends their own
//!   subscription instead of the box owner's. The model list is type-to-filter:
//!   typing narrows it by id or label (`deepseek` finds
//!   `opencode/deepseek-v4-flash`), arrows walk the matches, enter dispatches
//!   the highlighted one;
//! - every dispatch also carries who released it — this device's id and the
//!   signed-in user's email (gh#74). The engine records both on the attempt and
//!   names the human in the upstream comment. Claims, not credentials: board
//!   calls relay as the room owner, so the box cannot check them, and nothing
//!   is authorized on them.
//! - `f` cycles the routes on the board, `F` clears the filter, `/` opens the
//!   find field (live substring matching), `esc` closes the panel. The header's
//!   route chip is the same cycle for the mouse, and says which route the board
//!   is showing rather than that filtering exists (gh#295, claim E3);
//! - the `WatchBoard` subscription is **standing**, not lazy — the shell's
//!   sidebar draws its Agents section ([`BoardPanel::agents`], gh#103) off these
//!   rows with the dock shut — and reconnects with a 2 s backoff if the engine
//!   drops it.
//!
//! ## Which device's board (gh#55)
//!
//! The board store lives on ONE device — usually the always-on box — and every
//! board RPC is relay-forwardable, so this panel is not limited to a device
//! that hosts a board itself. It carries a *host*: `None` for this device, a
//! device id otherwise, merged into every board call as `targetDeviceId`
//! (`ListModels` included — the run executes on the host, so the catalog has to
//! be the host's).
//!
//! Finding the host needs no configuration. The engine refuses `WatchBoard`
//! outright when it hosts no board, so a candidate that ends its stream without
//! ever delivering a frame has answered "not me", and the watch loop walks the
//! candidates from `comet_proto::view::board::host_candidates`. A frame alone
//! does not settle the sweep (gh#125): a board with no dispatch evidence
//! (`board_dispatched`) is held as a fallback while the rest of the org is
//! asked, so a laptop's stale test board loses to the box everyone works from
//! — and still shows when nobody else answers. The title's "on {device}"
//! segment pins a device explicitly when the guess is wrong (or when two boxes
//! both host a board), and offers "Automatic" to hand the sweep back.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::Utc;
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable as _, IntoElement, KeyDownEvent,
    MouseButton, Render, ScrollHandle, SharedString, Subscription, Task, Window, actions, div,
    prelude::*, px,
};

use comet_board::autopick::AutomationsView;
use comet_proto::view::board::{self, BoardState, Filter, RowAction, TaskDetail, TaskRow};
use comet_proto::view::needs::{self as needs_view};
use comet_proto::{AgentAccount, AgentAccountsSnapshot, HarnessId};
use comet_rpc::methods;
use serde::Deserialize;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::icons::{self, icon};
use crate::motion;
use crate::popover;
use crate::state::{AppState, EngineHandle};
use crate::theme::{Bed, ListRow as _, Theme};

// One runtime a dispatch can be pointed at, as the engine's `ListBoardRuntimes`
// reports it — the same set `build_spec` validates an override against. Its
// `harness` is what tells the account strip which saved logins this runtime
// could actually spend (gh#74). The shape is proto's, like `TaskRow`: the panel
// deserializes what the board serves without depending on the board crate.
use comet_proto::view::board::RuntimeOption as BoardRuntime;

/// One model a dispatch can be pointed at, as the engine's `ListModels` reports
/// it for a harness — `id` is exactly what the `DispatchTask` override sends.
#[derive(Debug, Clone, Deserialize)]
struct BoardModelInfo {
    id: String,
    label: String,
}

/// Which of the picker's rows the keyboard highlights, top to bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerRow {
    Runtime,
    Account,
    Model,
}

/// A harness's model catalog in the dispatch picker, loaded for the highlighted
/// runtime on demand and cached (switching back is instant).
#[derive(Debug, Clone, Default)]
enum ModelCatalog {
    #[default]
    Idle,
    Loading,
    Ready(Vec<BoardModelInfo>),
    Error(String),
}

/// The host's saved agent logins, loaded once when the picker opens (gh#74).
/// One list for every harness — the account row filters it by the highlighted
/// runtime's, so switching runtimes needs no second call.
#[derive(Debug, Clone, Default)]
enum AccountCatalog {
    #[default]
    Loading,
    Ready(Vec<AgentAccount>),
    Error(String),
}

impl AccountCatalog {
    fn slots(&self) -> &[AgentAccount] {
        match self {
            AccountCatalog::Ready(accounts) => accounts,
            _ => &[],
        }
    }
}

/// A dispatch in the pick step: the operator chose a task, and the panel is
/// asking which runtime — and which model on that harness — to release it
/// under. Runtimes load asynchronously after the picker opens; keyboard nav is
/// inert until they land.
#[derive(Debug, Clone)]
struct DispatchDraft {
    task_id: String,
    identifier: String,
    /// The route's runtime — the picker's default, when the list offers it.
    route_runtime: Option<String>,
    runtimes: Vec<BoardRuntime>,
    active_runtime: usize,
    /// A failed `ListBoardRuntimes`, shown in the strip instead of a stale
    /// "Loading…" (enter still dispatches with the route's runtime).
    runtime_error: Option<String>,
    /// Per-runtime model catalogs, fetched for the highlighted runtime on
    /// demand (`ListModels { harness }` keyed by the canonical runtime name,
    /// which is exactly the harness's kebab-case id).
    catalogs: HashMap<String, ModelCatalog>,
    /// The row the keyboard highlights.
    row: PickerRow,
    /// The model highlight for the ACTIVE runtime: 0 = the harness default
    /// (no override — the route's behavior), `i` = the catalog's `i`-th model.
    active_model: usize,
    /// Every agent login the host has saved, whatever harness it belongs to.
    accounts: AccountCatalog,
    /// The account highlight: 0 = the route's own `account` (no override), `i`
    /// = the `i-1`th login the active runtime's harness can spend.
    active_account: usize,
    /// The route's account, for labelling row 0 — what "no override" will
    /// actually spend, so the default is a choice rather than a mystery.
    route_account: Option<String>,
    /// The email of whoever is signed in on THIS device, for the billing guard
    /// (gh#101). Snapshotted when the picker opens rather than read per frame:
    /// it is the fixed half of the comparison, and the chips are re-derived on
    /// every keystroke.
    viewer: Option<String>,
}

impl DispatchDraft {
    /// The canonical name of the highlighted runtime.
    fn active_runtime_name(&self) -> Option<String> {
        self.runtimes
            .get(self.active_runtime)
            .map(|r| r.name.clone())
    }

    /// The harness the highlighted runtime runs on. `None` before the runtime
    /// list lands — the account row has nothing to filter by until then.
    fn active_harness(&self) -> Option<HarnessId> {
        self.runtimes.get(self.active_runtime).map(|r| r.harness)
    }

    /// The model catalog of the highlighted runtime, when loaded.
    fn catalog(&self) -> Option<&ModelCatalog> {
        let name = self.active_runtime_name()?;
        self.catalogs.get(&name)
    }

    /// The logins the highlighted runtime could spend, in the order the host
    /// listed them.
    fn account_options(&self) -> Vec<&AgentAccount> {
        accounts_for_harness(self.accounts.slots(), self.active_harness())
    }

    /// What an account row has to say about whose subscription it spends
    /// (gh#101) — `None` when it spends the viewer's own, or when the picker
    /// cannot tell whose it is.
    ///
    /// `slot` is the agent-account id the row would send; `None` is row 0, the
    /// route's own default, which is the one an enter-enter release lands on
    /// without anybody having chosen it. That row is the whole reason this is
    /// resolved against the host's account list rather than shown as a slot id:
    /// `Route default · 8f2c1d0a` tells a teammate nothing about whose plan
    /// they are about to spend.
    fn bills(&self, slot: Option<&str>) -> Option<String> {
        let harness = self.active_harness()?;
        let billed = board::billed_email(self.accounts.slots(), harness, slot)?;
        board::cross_billed(Some(billed), self.viewer.as_deref())
            .then(|| board::bills_label(billed))
    }

    /// The account override this highlight sends, if any — carrying the label
    /// as well, so the confirmation can name whose limits the run will spend
    /// rather than echo a slot id.
    fn picked_account(&self) -> Option<PickedAccount> {
        let options = self.account_options();
        let id = override_account_id(&options, self.active_account)?.to_string();
        let label = options
            // Row 0 is the route's, and returned `None` above.
            .get(self.active_account - 1)
            .map(|account| account_label(account))
            .unwrap_or_else(|| id.clone());
        Some(PickedAccount { id, label })
    }

    /// Everything this release sends, from the three highlights. `catalog_ix`
    /// is the model row being confirmed — the highlighted one for enter, the
    /// clicked one for a click, and `None` when the catalog never landed (the
    /// dispatch then goes out on the harness default, as it always could).
    fn choice(&self, catalog_ix: Option<usize>) -> DispatchChoice {
        let (model, effective_model) = match (self.catalog(), catalog_ix) {
            (Some(ModelCatalog::Ready(models)), Some(ix)) => (
                override_model_id(models, ix).map(str::to_string),
                models.get(ix).map(|m| m.id.clone()),
            ),
            _ => (None, None),
        };
        DispatchChoice {
            runtime: self.active_runtime_name(),
            model,
            effective_model,
            account: self.picked_account(),
            // Never on the first send: the guard exists to make somebody say it
            // out loud, and a picker that pre-consented would be the picker
            // ticking the box for them.
            bill: None,
        }
    }

    /// Who the highlighted selection charges, whoever that is — the payer the
    /// confirm has to name if the host refuses this release (gh#101).
    fn billed_to(&self) -> Option<String> {
        let harness = self.active_harness()?;
        let slot = self
            .picked_account()
            .map(|a| a.id)
            .or_else(|| self.route_account.clone());
        board::billed_email(self.accounts.slots(), harness, slot.as_deref()).map(str::to_string)
    }
}

/// An account chosen in the picker: what to send, and what to call it.
#[derive(Debug, Clone)]
struct PickedAccount {
    id: String,
    label: String,
}

/// What the picker settled on for one release. A struct rather than four
/// positional `Option<String>`s: they are all optional and all strings, so a
/// swapped pair would be a silently wrong dispatch rather than a type error.
#[derive(Debug, Clone, Default)]
struct DispatchChoice {
    /// Runtime override; `None` = the route's.
    runtime: Option<String>,
    /// Model override; `None` = the harness default.
    model: Option<String>,
    /// The model the run will actually use — the override, else the default
    /// the catalog leads with. For the confirmation, never sent.
    effective_model: Option<String>,
    /// Account override; `None` = the route's account.
    account: Option<PickedAccount>,
    /// "Bill that account, I know whose it is" — set only on the second send,
    /// after the host refused the first under `require-own` (gh#101). Names
    /// the payer, because a consent that does not say what it consents to is
    /// a checkbox people tick once.
    bill: Option<String>,
}

/// A refused release, kept so `enter` can mean "yes, bill them" (gh#101).
#[derive(Debug, Clone)]
struct PendingBill {
    task_id: String,
    identifier: String,
    /// Everything the picker settled on, replayed verbatim — the second send
    /// must spend the same account the first one was refused for, or the
    /// confirmation named something the dispatch did not do.
    choice: DispatchChoice,
    /// Who the run charges, as the picker resolved it. What the confirm says
    /// out loud and what rides on the retry as `bill`.
    billed_to: String,
}

/// The saved logins a harness can spend. A Claude slot is not lendable to a
/// codex run — the two harnesses read different config-dir variables — so a
/// picker that offered every slot for every runtime would be offering dispatches
/// that refuse themselves (gh#59). An unknown harness (the runtime list has not
/// landed yet) matches nothing rather than everything.
fn accounts_for_harness(
    accounts: &[AgentAccount],
    harness: Option<HarnessId>,
) -> Vec<&AgentAccount> {
    let Some(harness) = harness else {
        return Vec::new();
    };
    accounts.iter().filter(|a| a.harness == harness).collect()
}

/// The `account` a dispatch should SEND for an account highlight: `None` at row
/// 0 — the route's own account, which needs no override — and the slot id of the
/// `i-1`th offered login otherwise.
fn override_account_id<'a>(options: &[&'a AgentAccount], active: usize) -> Option<&'a str> {
    match active {
        0 => None,
        k => options.get(k - 1).map(|a| a.id.as_str()),
    }
}

/// What an account chip says: the login's email, else the name the harness
/// reported, else the slot id — never nothing, since the chip is how the
/// operator tells whose limits a dispatch will spend.
fn account_label(account: &AgentAccount) -> String {
    account
        .email
        .clone()
        .or_else(|| account.display_name.clone())
        .unwrap_or_else(|| account.id.clone())
}

/// The picker's starting row: the route's runtime when the list offers it by
/// its canonical name, else the first option the host can actually start. A
/// route configured with an alias (`claude`, `openai-codex`) lands on its
/// harness's canonical entry.
///
/// The route's own runtime wins even when the host cannot start it (gh#187).
/// That is not an oversight: the picker's job is to say what a dispatch would
/// do, and "the route sends this to OpenCode, which is not installed on the
/// box" is precisely the sentence an operator needs to read. Only the
/// *fallback* prefers an available option — landing on an unavailable one
/// nobody chose would be the picker inventing a dead end.
fn default_runtime_index(options: &[BoardRuntime], route_runtime: Option<&str>) -> usize {
    route_runtime
        .and_then(|route| options.iter().position(|o| o.name == route))
        .or_else(|| options.iter().position(|o| o.available()))
        .unwrap_or(0)
}

/// Indices of the active runtime's catalog models matching the dispatch
/// picker's query — matched against the model id OR its display label, best
/// (prefix-over-substring) rank per model, stable in catalog order. An empty
/// query matches everything: the whole catalog in order, so row 0 stays the
/// harness default (no override). This is the board-side twin of the
/// composer's model filter (`pickers.rs`).
fn filtered_model_indices(models: &[BoardModelInfo], query: &str) -> Vec<usize> {
    if query.trim().is_empty() {
        return (0..models.len()).collect();
    }
    let mut ranked: Vec<(usize, usize)> = models
        .iter()
        .enumerate()
        .filter_map(|(ix, m)| {
            [m.id.as_str(), m.label.as_str()]
                .into_iter()
                .filter_map(|field| popover::match_rank(query, field))
                .min()
                .map(|rank| (rank, ix))
        })
        .collect();
    ranked.sort_by_key(|&(rank, ix)| (rank, ix));
    ranked.into_iter().map(|(_, ix)| ix).collect()
}

/// The override a dispatch should SEND for a highlight: `None` at the default
/// row (the harness default needs no override — the route's behavior), the
/// catalog model's id otherwise.
fn override_model_id(models: &[BoardModelInfo], active: usize) -> Option<&str> {
    match active {
        0 => None,
        k => models.get(k).map(|m| m.id.as_str()),
    }
}

/// Merge the board host's `targetDeviceId` passthrough into a call's params
/// (gh#55). `None` — the board is this device's — leaves the params untouched,
/// so a single-device install sends exactly the shape it always did.
fn host_params(host: Option<&str>, value: serde_json::Value) -> serde_json::Value {
    let mut value = value;
    if let (Some(host), Some(object)) = (host, value.as_object_mut()) {
        object.insert("targetDeviceId".into(), serde_json::json!(host));
    }
    value
}

/// Whether the dispatch picker consumes a keystroke instead of letting it
/// reach the board frame's key handler. Navigation, confirm and cancel keys
/// are always the picker's; while the model search is focused a plain
/// printable key (e.g. `f`) is the input's too, so it cannot fall through and
/// cycle the board filter mid-typing — the same guard the find field gets via
/// `model.typing`.
fn dispatch_picker_owns_key(key: &str, search_focused: bool) -> bool {
    match key {
        "up" | "down" | "left" | "right" | "enter" | "escape" => true,
        _ => search_focused,
    }
}

/// A line the board body draws: a section header, a route's group header
/// within it (gh#125), or a task row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoardLine {
    Section(BoardState),
    /// One route's group inside a section. `None` is the `no route` group.
    Group(BoardState, Option<String>),
    Task(String),
}

impl BoardLine {
    /// The selection id this line answers to. Header ids carry a NUL prefix so
    /// they can never collide with a task id (same convention as the TUI).
    fn id(&self) -> String {
        match self {
            BoardLine::Section(state) => section_row_id(*state),
            BoardLine::Group(state, route) => group_row_id(*state, route.as_deref()),
            BoardLine::Task(id) => id.clone(),
        }
    }
}

/// Selection id for a section header. The NUL prefix keeps it out of the task
/// id space.
fn section_row_id(state: BoardState) -> String {
    format!("\u{0}section:{}", state.as_str())
}

/// Selection id for a route group's header, under the same NUL convention.
fn group_row_id(state: BoardState, route: Option<&str>) -> String {
    format!(
        "\u{0}group:{}:{}",
        state.as_str(),
        route.unwrap_or(board::NO_ROUTE)
    )
}

/// A task row's height — **the same for every row, in every state** (gh#132),
/// and one line tall (gh#176).
///
/// gh#125 made it a minimum so the hovered row could wrap its title, which is
/// how a hover came to reflow everything under it. gh#132 made it a constant
/// again — 47px: a title line, a metadata line, and the gap between them, held
/// whether or not the row had any metadata to put on the second one. Ready rows
/// mostly have none, so a third of the list's vertical budget was reserved
/// blank by contract.
///
/// gh#176 spends it: the metadata moved onto the title's line as a right-hand
/// column, and the row is 32px. The guarantee is untouched and the arithmetic
/// is simpler for it — one line, its padding, and nothing a pointer does may
/// change either.
const ROW_H: f32 = ROW_PAD_Y * 2.0 + ROW_LINE_H;
/// The row's one line — the action chip's height (E9), so a row showing chips
/// is exactly as tall as one that is not.
const ROW_LINE_H: f32 = 22.0;
/// The row's vertical padding, named because [`ROW_H`] is built from it and a
/// row whose declared height disagreed with its content would clip the line.
const ROW_PAD_Y: f32 = 5.0;
/// The panel's own side padding — its header, every group header, every row and
/// the footer sit on it (E2, E4, E6, E14).
///
/// Fourteen is not a step on the spacing scale ([`Theme::SPACE_MD`] is 12 and
/// [`Theme::SPACE_LG`] is 16), and `tokens.md` has no name for it, so it is a
/// constant here rather than the nearest token wearing the wrong meaning.
const PAD_X: f32 = 14.0;
/// The row's fixed columns (E6), in the order they are drawn. The title takes
/// whatever is left, which is why the three facts beside it are columns to read
/// down rather than a sentence that ends wherever the title stopped.
const COL_GLYPH: f32 = 10.0;
const COL_ID: f32 = 56.0;
const COL_REPO: f32 = 76.0;
const COL_TIME: f32 = 60.0;
/// How tall the peek's issue body may get before it scrolls (gh#132). Capped
/// rather than proportional: the list above is the thing being navigated, and a
/// panel that grew with a long issue would push the cursor's own row off screen.
const PEEK_BODY_MAX_H: f32 = 220.0;

/// The accent a board state carries — the status ramp's answer, never this
/// file's (gh#173). Blocked and failed share red (the glyph tells them apart),
/// working is amber, review indigo; ready and done spend no colour, so they
/// land on the row's own text tones — plain for a queued row, dim for history.
pub(crate) fn state_color(state: BoardState, theme: &Theme) -> gpui::Hsla {
    match crate::theme::Status::of_board(state) {
        Some(status) => theme.status(status),
        None if state == BoardState::Done => theme.text_faint,
        None => theme.text,
    }
}

/// A section header's words on this surface (gh#176).
///
/// [`BoardState::label`] is the published spelling — `BLOCKED`, `DONE TODAY` —
/// and it stays that way: the TUI, the CLI and the phone all say it, and a
/// contract is not something one viewport edits. Caps are a *typographic*
/// choice, though, and this surface has stopped making it: a header shouting in
/// a grey slab was loud without being clear. Same words, sentence case.
fn section_title(state: BoardState) -> &'static str {
    match state {
        BoardState::Blocked => "Blocked",
        BoardState::Working => "Working",
        BoardState::Ready => "Ready",
        BoardState::Review => "Review",
        BoardState::Failed => "Failed",
        BoardState::Done => "Done today",
    }
}

/// A stable element-id fragment per action — ids must be unique and constant
/// across frames, and the label is not (it has a short form).
fn action_key(action: RowAction) -> &'static str {
    match action {
        RowAction::Dispatch => "dispatch",
        RowAction::Retry => "retry",
        RowAction::Cancel => "cancel",
        RowAction::OpenChat => "chat",
        RowAction::OpenIssue => "issue",
        RowAction::OpenPr => "pr",
    }
}

/// What an action is coloured: the release reads as the row's own text, ending
/// somebody's work reads as danger, a link reads as a link, and opening the
/// chat stays quiet beside whichever of those it sits next to.
fn action_color(action: RowAction, theme: &Theme) -> gpui::Hsla {
    match action {
        RowAction::Dispatch | RowAction::Retry => theme.text,
        RowAction::Cancel => theme.danger,
        RowAction::OpenPr => theme.accent,
        RowAction::OpenChat | RowAction::OpenIssue => theme.text_muted,
    }
}

/// The accent a *live agent* carries in the sidebar — routed through the same
/// status ramp as [`state_color`], so a running attempt does not change colour
/// on its way from the board pane to the sidebar (gh#103, gh#173).
pub fn agent_state_color(state: board::AgentState, theme: &Theme) -> gpui::Hsla {
    theme.status(crate::theme::Status::of_agent(state))
}

/// The repo column (E6): the repository the row's work lands in.
///
/// A GitHub id carries it (`gh:Florin-AS/tally#507` → `tally`), and it is the
/// half of the identifier that makes `#507` mean anything — which is why the id
/// column can go back to the upstream spelling now that the repo has a column
/// of its own. A Linear id names no repo, so the row says where the work runs
/// instead: the space, which is the only answer that source has.
fn repo_cell(row: &TaskRow) -> String {
    board::gh_repo_name(&row.id)
        .map(str::to_string)
        .or_else(|| row.workspace.clone())
        .unwrap_or_default()
}

/// The time column (E6): the one fact a row's own state measures itself by.
///
/// A live attempt is a clock; a review is the pull request you would open; a
/// failed attempt is how it ended; a closed row is the agent that did it (E13).
/// A ready row has nothing running and so nothing to say here, and says
/// nothing — the column stays a column either way.
fn time_cell(row: &TaskRow, now: chrono::DateTime<Utc>) -> String {
    match row.state() {
        BoardState::Working | BoardState::Blocked => row
            .started_at
            .as_deref()
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            // Never negative: clock skew must not render as a count-up from
            // the future (the same guard the shared derivation keeps).
            .map(|start| {
                board::format_elapsed((now - start.with_timezone(&Utc)).num_seconds().max(0))
            })
            .unwrap_or_default(),
        BoardState::Review => match row.pr_number {
            Some(number) => format!("PR #{number}"),
            None => "no PR".to_string(),
        },
        BoardState::Failed => "exited".to_string(),
        BoardState::Ready => String::new(),
        BoardState::Done => row.runtime.clone().unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// Pure interaction model (unit-tested)
// ---------------------------------------------------------------------------

/// The panel's view state, independent of gpui. The row derivations come from
/// [`board`]; this owns only what the derivations need a home for.
#[derive(Debug, Default)]
pub struct BoardModel {
    pub rows: Vec<TaskRow>,
    pub filter: Filter,
    /// The `/` field is open and taking keys. Implies [`Filter::Text`].
    pub typing: bool,
    pub selected: Option<String>,
    /// Sections folded away. `done` starts folded: it is history, and the rest
    /// is the queue.
    pub collapsed: HashSet<BoardState>,
    /// Per-group fold overrides (gh#125). Absent means the group's default —
    /// open for a named route, folded for `no route` — so a group the operator
    /// never touched keeps following the rule as it comes and goes.
    pub group_folds: HashMap<(BoardState, Option<String>), bool>,
}

impl BoardModel {
    /// `done` starts folded — a fresh board shows the queue, not history.
    pub fn new() -> Self {
        Self {
            rows: Vec::new(),
            filter: Filter::All,
            typing: false,
            selected: None,
            collapsed: HashSet::from([BoardState::Done]),
            group_folds: HashMap::new(),
        }
    }

    /// Replace the rows from a watch frame. Selection persists by id, not
    /// index, so a poll landing on a row must not move the cursor.
    pub fn set_rows(&mut self, rows: Vec<TaskRow>) {
        self.rows = rows;
        self.clamp_selection();
    }

    /// Sections in fixed order, empty ones omitted, the filter applied.
    pub fn sections(&self) -> Vec<(BoardState, Vec<&TaskRow>)> {
        board::sections(&self.rows, &self.filter, Utc::now())
    }

    /// The sections with each one's rows grouped by route (gh#125).
    pub fn grouped(&self) -> Vec<(BoardState, Vec<board::SectionGroup<'_>>)> {
        board::grouped_sections(&self.rows, &self.filter, Utc::now())
    }

    /// Every line the body draws, in display order. Group headers appear only
    /// where the shared rule says they earn their row.
    pub fn lines(&self) -> Vec<BoardLine> {
        let mut out = Vec::new();
        for (state, groups) in self.grouped() {
            out.push(BoardLine::Section(state));
            if self.collapsed.contains(&state) {
                continue;
            }
            let headers = board::group_headers_shown(&self.filter, &groups);
            for group in groups {
                if headers {
                    out.push(BoardLine::Group(state, group.route.clone()));
                    if self.is_group_collapsed(state, group.route.as_deref()) {
                        continue;
                    }
                }
                out.extend(group.rows.iter().map(|row| BoardLine::Task(row.id.clone())));
            }
        }
        out
    }

    pub fn task(&self, id: &str) -> Option<&TaskRow> {
        self.rows.iter().find(|row| row.id == id)
    }

    /// The selected task, or `None` — including when the cursor is on a
    /// section header, which is a line but not a task. A filtered-away row is
    /// not selected, however the cursor came to be on it.
    pub fn selected_task(&self) -> Option<&TaskRow> {
        let id = self.selected.as_deref()?;
        self.task(id).filter(|row| self.filter.matches(row))
    }

    /// The section header the cursor is on, if any.
    pub fn on_section(&self) -> Option<BoardState> {
        let id = self.selected.as_deref()?;
        BoardState::SECTION_ORDER
            .into_iter()
            .find(|state| section_row_id(*state) == id)
    }

    pub fn is_collapsed(&self, state: BoardState) -> bool {
        self.collapsed.contains(&state)
    }

    pub fn toggle_collapsed(&mut self, state: BoardState) {
        if !self.collapsed.remove(&state) {
            self.collapsed.insert(state);
        }
    }

    /// The group header the cursor is on, if any.
    pub fn on_group(&self) -> Option<(BoardState, Option<String>)> {
        let id = self.selected.as_deref()?;
        self.lines().into_iter().find_map(|line| match line {
            BoardLine::Group(state, route) if group_row_id(state, route.as_deref()) == id => {
                Some((state, route))
            }
            _ => None,
        })
    }

    /// Folded, by the operator's override or the group's default: a named
    /// route's group is open, the `no route` group starts folded on the
    /// unfiltered board (gh#125).
    pub fn is_group_collapsed(&self, state: BoardState, route: Option<&str>) -> bool {
        self.group_folds
            .get(&(state, route.map(str::to_string)))
            .copied()
            .unwrap_or_else(|| board::group_starts_collapsed(&self.filter, route))
    }

    pub fn toggle_group(&mut self, state: BoardState, route: Option<&str>) {
        let folded = self.is_group_collapsed(state, route);
        self.group_folds
            .insert((state, route.map(str::to_string)), !folded);
    }

    /// How many rows a group holds, for the count on its header.
    pub fn group_len(&self, state: BoardState, route: Option<&str>) -> usize {
        self.grouped()
            .into_iter()
            .find(|(s, _)| *s == state)
            .and_then(|(_, groups)| {
                groups
                    .into_iter()
                    .find(|g| g.route.as_deref() == route)
                    .map(|g| g.rows.len())
            })
            .unwrap_or(0)
    }

    /// How many rows a section holds, for the count on a folded header.
    pub fn section_len(&self, state: BoardState) -> usize {
        self.sections()
            .into_iter()
            .find(|(s, _)| *s == state)
            .map(|(_, rows)| rows.len())
            .unwrap_or(0)
    }

    /// Move the selection by `delta` lines, clamped at both ends.
    pub fn select_delta(&mut self, delta: isize) {
        let ids = self.lines();
        if ids.is_empty() {
            self.selected = None;
            return;
        }
        let cur = self
            .selected
            .as_deref()
            .and_then(|id| ids.iter().position(|line| line.id() == id))
            .unwrap_or(0);
        let next = (cur as isize + delta).clamp(0, ids.len() as isize - 1) as usize;
        self.selected = Some(ids[next].id());
    }

    /// `f` — the next position, then back to all. Returns a message to flash
    /// when the cycle is empty (an empty board has nothing to say about routes).
    pub fn cycle_filter(&mut self) -> Option<String> {
        let cycle = board::filter_cycle(&self.rows, Utc::now());
        if cycle.is_empty() {
            self.clear_filter();
            return Some("nothing on the board to filter".into());
        }
        let next = match &self.filter {
            cur @ (Filter::Route(_) | Filter::NoRoute) => {
                match cycle.iter().position(|candidate| candidate == cur) {
                    Some(i) => i + 1,
                    // The position left the board while it was filtering; start
                    // again rather than dropping the operator somewhere
                    // arbitrary.
                    None => 0,
                }
            }
            _ => 0,
        };
        self.filter = cycle.get(next).cloned().unwrap_or(Filter::All);
        self.typing = false;
        self.clamp_selection();
        None
    }

    /// `F` — clear whichever filter is active.
    pub fn clear_filter(&mut self) {
        self.filter = Filter::All;
        self.typing = false;
        self.clamp_selection();
    }

    /// `/` — open the text field. Matching is live, so an empty query is the
    /// whole board rather than nothing.
    pub fn open_find(&mut self) {
        self.filter = Filter::Text(String::new());
        self.typing = true;
        self.clamp_selection();
    }

    pub fn find_type(&mut self, ch: char) {
        match &mut self.filter {
            Filter::Text(q) => q.push(ch),
            _ => self.filter = Filter::Text(ch.to_string()),
        }
        self.typing = true;
        self.clamp_selection();
    }

    /// `enter` on the field: keep the query, close the field, hand the keys
    /// back to the board.
    pub fn accept_find(&mut self) {
        self.typing = false;
        if matches!(&self.filter, Filter::Text(q) if q.trim().is_empty()) {
            self.filter = Filter::All;
        }
        self.clamp_selection();
    }

    /// `esc` on the field: clear the query and close it. The filter was typed
    /// for this moment; it is not worth keeping on a board that looks broken.
    pub fn escape_find(&mut self) {
        self.clear_filter();
    }

    /// Keep the cursor on a line that is actually on screen.
    ///
    /// Called after a refresh and after every filter change: the line under the
    /// cursor being filtered away is the ordinary case, not an edge one. When
    /// the cursor has to move, it lands on the first TASK (a section header is
    /// a line, not something to dispatch).
    fn clamp_selection(&mut self) {
        let ids = self.lines();
        let keep = self
            .selected
            .as_deref()
            .is_some_and(|id| ids.iter().any(|line| line.id() == id));
        if !keep {
            self.selected = ids
                .iter()
                .find(|line| matches!(line, BoardLine::Task(_)))
                .map(|line| line.id())
                .or_else(|| ids.first().map(|line| line.id()));
        }
    }

    /// What to say when the filter has hidden every row.
    pub fn empty_note(&self) -> Option<String> {
        self.filter.empty_note()
    }
}

// ---------------------------------------------------------------------------
// Entity
// ---------------------------------------------------------------------------

/// The board panel's one outward verb.
///
/// Everything else the panel does it does itself — dispatch, cancel, select a
/// chat — because those are board operations against the board's host. Opening
/// a review is not: it is a **route change**, which belongs to the shell, and a
/// panel that reached over and set the shell's route would be a dock that owns
/// the window.
#[derive(Debug, Clone)]
pub enum BoardEvent {
    /// Show one attempt's review (gh#180), with the chat that authored it in
    /// the column beside it. `chat_id` is `None` where the attempt's chat is
    /// gone — the review outlives it, which is why the claims live on the
    /// attempt rather than in the transcript.
    OpenReview {
        task_id: String,
        chat_id: Option<String>,
    },
    /// Open Settings → Automations (gh#490) — the popover's "Manage
    /// automations…" deep link. A route change, so the shell's, exactly as
    /// opening a review is.
    OpenAutomations,
}

/// The board dock, and the shell's standing source of board rows.
///
/// The watch used to be lazy — no RPC until the dock was first opened. It is
/// standing since gh#103: the sidebar's Agents section is drawn from these rows
/// whether or not the dock has ever been open, and a presence list that only
/// works after you have visited the board is not presence. The cost is the host
/// sweep on a device with no board, which is bounded (`host_candidates` is asked
/// once each, then a 2 s backoff) and is what the TUI has always done.
pub struct BoardPanel {
    state: Entity<AppState>,
    focus_handle: FocusHandle,
    open: bool,
    model: BoardModel,
    /// The watch has been attempted — `ensure_watch` is a one-way door so a
    /// reconnect loop owns the lifecycle, not the shell's toggle.
    started: bool,
    error: Option<SharedString>,
    watch_task: Option<Task<()>>,
    /// The `/` field, when open.
    find: Option<Entity<ComposerInput>>,
    _find_events: Option<Subscription>,
    /// Focus the find field on its first paint (opened without window access).
    find_focus_pending: bool,
    /// The dispatch picker's model filter (PaletteSearch context, so ↑↓/←→/
    /// enter bubble to the board frame's dispatch handling).
    dispatch_search: Entity<ComposerInput>,
    _dispatch_search_events: Subscription,
    /// The board body's scroll position.
    scroll: ScrollHandle,
    /// The model row's scroll position (the catalog is long — opencode alone
    /// offers 70+ models — so the strip scrolls horizontally).
    model_scroll: ScrollHandle,
    /// A transient dispatch/cancel message for the footer.
    notice: Option<SharedString>,
    /// Which device's board this is: `None` = this device (no passthrough).
    /// Walked by the watch loop until a device answers, unless pinned.
    host: Option<String>,
    /// The operator picked the host from the chip — the sweep stops guessing
    /// and a silent device stays selected (with a banner) instead of being
    /// walked past.
    host_pinned: bool,
    /// The current host has delivered at least one frame the sweep settled on,
    /// so it really is this panel's board. Drives the title's presence dot.
    host_confirmed: bool,
    /// A host the automatic sweep held instead of settling on (gh#125): it
    /// delivered a frame, but a board with no dispatch evidence must lose to
    /// the org's active host if one answers. Used when the sweep exhausts.
    sweep_fallback: Option<Option<String>>,
    /// The sweep is returning to its held fallback: nobody with dispatch
    /// evidence answered, so the next frame settles regardless.
    sweep_settling: bool,
    /// `COMET_OPEN_REVIEW=1` has already fired. One-way: the knob opens the
    /// first review it sees and then leaves the window alone, or every board
    /// frame would drag the operator back out of wherever they navigated.
    review_on_boot_done: bool,
    host_menu_open: bool,
    /// Outside-click dismissal instant — suppresses the trigger click that
    /// follows the same mouse-down from instantly reopening the menu.
    host_menu_dismissed_at: Option<std::time::Instant>,
    /// The Automations popover (gh#490): whether it is open, the same
    /// dismissal guard the host menu carries, the last view the host answered
    /// with, and the in-flight fetch or pause/resume write.
    automations_open: bool,
    automations_dismissed_at: Option<std::time::Instant>,
    automations: Option<AutomationsView>,
    automations_task: Option<Task<()>>,
    /// An open runtime picker for a dispatch (ready row → enter). `None` =
    /// no dispatch is being confirmed.
    dispatch: Option<DispatchDraft>,
    /// A release the host refused because it would spend somebody else's
    /// subscription (`billing_guard = "require-own"`, gh#101), held so `enter`
    /// can send it again naming the payer.
    ///
    /// The confirm the CLI spells `--bill`. Reactive rather than pre-emptive on
    /// purpose: the mode lives in the host's `routing.toml` and this panel does
    /// not read it, so the only honest way to ask "do you mean it" is to ask
    /// after the box has said it minds.
    pending_bill: Option<PendingBill>,
    /// The peek panel is showing the selected row (gh#132). Sticky: it follows
    /// the cursor once opened, so walking the board reads rather than needing a
    /// keypress per row, and `space` (or escape) shuts it again.
    peek: bool,
    /// The issue text behind the row the peek last asked for, keyed by task id
    /// so a reply that raced a newer selection is dropped rather than rendered
    /// under the wrong title.
    detail: Option<TaskDetail>,
    /// The task a `ReadBoardTask` is in flight for — the fetch is idempotent
    /// per row, and re-asking on every frame would be a call per repaint.
    detail_pending: Option<String>,
    /// Why the last detail fetch failed, said where the body would be.
    detail_error: Option<SharedString>,
    /// The peek's own scroll: an issue body is longer than the panel.
    detail_scroll: ScrollHandle,
    /// Keeps the elapsed counters on working/blocked rows live.
    _ticker: Task<()>,
    _observe: Subscription,
}

impl BoardPanel {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        // Start (or restart) the watch as soon as the engine exists — the panel
        // may never be opened, and the sidebar's Agents section still wants rows.
        let observe = cx.observe(&state, |this: &mut Self, _, cx| {
            this.ensure_watch(cx);
        });
        let dispatch_search =
            cx.new(|cx| ComposerInput::with_context("Search models…", "PaletteSearch", cx));
        // Type-to-filter: a fresh query re-homes the model highlight on the
        // first match (the Edited event fires on every keystroke).
        let dispatch_search_events = cx.subscribe(
            &dispatch_search,
            |this: &mut Self, _, event, cx| match event {
                ComposerInputEvent::Edited(_) => {
                    if let Some(draft) = this.dispatch.as_mut() {
                        draft.active_model = 0;
                    }
                    cx.notify();
                }
                // No `/` or `@` picker on a dispatch search box, so its
                // navigation keys never become menu events — and ⌘-Enter has
                // nothing to queue.
                ComposerInputEvent::QueueSubmitted
                | ComposerInputEvent::Menu(_)
                | ComposerInputEvent::Submitted
                | ComposerInputEvent::PastedImages(_)
                | ComposerInputEvent::PastedPaths(_) => {}
            },
        );
        let ticker = cx.spawn(async move |this, cx| {
            // Whether the last tick found anything live. An unmanaged row's
            // membership is staleness-gated, and staleness passes with no frame
            // to announce it — a backend that died mid-run sends nothing ever
            // again. Redrawing one more time after the last live thing goes
            // quiet is what paints the frame the row is gone from; without it
            // a crashed run would sit in the sidebar until something unrelated
            // moved. It also bounds the ticking: quiet stays quiet.
            let mut was_live = false;
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let alive = this.update(cx, |panel, cx| {
                    // Keep the elapsed counters live while something is running
                    // — on the board pane, and on the sidebar's Active rows,
                    // which are drawn off the same rows with the dock shut.
                    let attempts = panel.model.rows.iter().any(|row| row.state().holds_pane());
                    // ...and on the unmanaged rows (gh#117), which no board row
                    // accounts for: a box hosting no board at all can still
                    // have a counter on screen that has to move.
                    let now = Utc::now();
                    let unmanaged = panel.state.read(cx).sessions.iter().any(|session| {
                        matches!(
                            comet_proto::view::effective_indicator(Some(session), now),
                            comet_proto::view::Indicator::Working
                                | comet_proto::view::Indicator::AwaitingInput
                        )
                    });
                    let live = attempts || unmanaged;
                    if live || was_live {
                        cx.notify();
                    }
                    was_live = live;
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        Self {
            state,
            focus_handle: cx.focus_handle(),
            open: false,
            model: BoardModel::new(),
            started: false,
            error: None,
            watch_task: None,
            find: None,
            _find_events: None,
            find_focus_pending: false,
            dispatch_search,
            _dispatch_search_events: dispatch_search_events,
            scroll: ScrollHandle::new(),
            model_scroll: ScrollHandle::new(),
            notice: None,
            host: None,
            host_pinned: false,
            host_confirmed: false,
            sweep_fallback: None,
            sweep_settling: false,
            review_on_boot_done: false,
            host_menu_open: false,
            host_menu_dismissed_at: None,
            automations_open: false,
            automations_dismissed_at: None,
            automations: None,
            automations_task: None,
            dispatch: None,
            pending_bill: None,
            peek: false,
            detail: None,
            detail_pending: None,
            detail_error: None,
            detail_scroll: ScrollHandle::new(),
            _ticker: ticker,
            _observe: observe,
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    /// Everything alive, joined to this device's chats and sessions — what the
    /// sidebar's Active group draws (gh#103's attempts and gh#117's unmanaged
    /// runs, one list since gh#123).
    ///
    /// On the panel rather than in `AppState` because the panel is what holds a
    /// board subscription, host sweep included: there is exactly one place that
    /// knows which device's rows these are, and a second copy in app state would
    /// be a second thing to keep pointed at the same box. A device hosting no
    /// board still answers — `self.model.rows` is empty, nothing is subtracted,
    /// and every live chat is an unmanaged row.
    pub fn active(&self, cx: &App, now: chrono::DateTime<Utc>) -> Vec<board::ActiveRow> {
        let state = self.state.read(cx);
        board::active_rows(
            &self.model.rows,
            &state.chats,
            &state.sessions,
            state.orchestrator.as_deref(),
            now,
        )
    }

    /// The "Needs you" inbox (gh#122): everything waiting on a human, as rows
    /// that say who and what in words. Here for the same reason
    /// [`BoardPanel::active`] is — the panel is the one holder of this
    /// device-swept board's rows, and the inbox joins them to the pin, the
    /// chats and the session watch.
    pub fn needs(&self, cx: &App, now: chrono::DateTime<Utc>) -> Vec<needs_view::NeedRow> {
        let state = self.state.read(cx);
        needs_view::needs_you(
            state.orchestrator.as_deref(),
            &self.model.rows,
            &state.chats,
            &state.sessions,
            now,
        )
    }

    /// Shell toggle hook. Opening starts the watch; closing keeps the rows so
    /// the next open lands instantly (the stream re-fetches on reconnect).
    pub fn set_open(&mut self, open: bool, cx: &mut Context<Self>) {
        self.open = open;
        if open {
            self.ensure_watch(cx);
        }
        cx.notify();
    }

    fn engine(&self, cx: &App) -> Option<EngineHandle> {
        self.state.read(cx).engine().cloned()
    }

    // ---- which device's board (gh#55) ----

    /// Merge the `targetDeviceId` passthrough into a board call's params. A
    /// no-op while the board is this device's, so a single-device install
    /// sends exactly what it always did.
    fn host_params(&self, value: serde_json::Value) -> serde_json::Value {
        host_params(self.host.as_deref(), value)
    }

    /// The host's display name, for the header and the banners.
    ///
    /// The local host is named too, not called "This device" (E2): the header
    /// says which box produced these rows, and "this device" is the one answer
    /// that stops meaning anything the moment a screenshot of it is read
    /// somewhere else. Falls back to the pronoun only when the device list has
    /// not landed yet.
    fn host_label(&self, cx: &App) -> SharedString {
        let state = self.state.read(cx);
        let id = self
            .host
            .as_deref()
            .or(state.local_device_id.as_deref())
            .unwrap_or_default();
        state
            .devices
            .iter()
            .find(|d| d.id == id)
            .map(|d| SharedString::from(d.name.clone()))
            .unwrap_or_else(|| match self.host.as_deref() {
                // A host id with no device row (the row has not synced yet) is
                // still worth naming — the id is what the call carries.
                Some(host) => SharedString::from(host.to_string()),
                None => "This device".into(),
            })
    }

    /// Point the panel at a device, explicitly. `pinned` stops the automatic
    /// sweep — an operator who chose a device wants that device's board, or a
    /// clear reason it has none, not a silent hop to another one.
    fn set_host(&mut self, host: Option<String>, pinned: bool, cx: &mut Context<Self>) {
        self.host_menu_open = false;
        if self.host == host && self.host_pinned == pinned {
            cx.notify();
            return;
        }
        self.host = host;
        self.host_pinned = pinned;
        self.host_confirmed = false;
        self.sweep_fallback = None;
        self.sweep_settling = false;
        self.error = None;
        // A different device is a different board: drop the rows rather than
        // leave another box's tasks on screen under the new host's name.
        self.model.set_rows(Vec::new());
        self.dispatch = None;
        if let Some(engine) = self.engine(cx) {
            self.started = true;
            // Replacing the task drops (and so cancels) the old subscription.
            self.watch_task = Some(Self::spawn_watch(engine, cx));
        }
        cx.notify();
    }

    /// The watch loop's verdict on a frame from the current host. Returns true
    /// when the sweep should move on instead of settling here (gh#125): an
    /// automatic sweep holds a board with no dispatch evidence as a fallback
    /// while other candidates remain unasked — a local board that exists but
    /// was never dispatched from must lose to the org's active host.
    fn host_frame(&mut self, rows: Vec<TaskRow>, cx: &mut Context<Self>) -> bool {
        self.error = None;
        if !self.host_confirmed {
            if !self.host_pinned && !self.sweep_settling && !board::board_dispatched(&rows) {
                let (devices, local) = {
                    let state = self.state.read(cx);
                    (state.devices.clone(), state.local_device_id.clone())
                };
                let candidates = board::host_candidates(&devices, local.as_deref());
                if let Some(next) = board::next_host_candidate(&candidates, self.host.as_deref()) {
                    // First held answer wins the fallback slot — it is the
                    // earliest in sweep order, which is the old tie-break.
                    if self.sweep_fallback.is_none() {
                        self.sweep_fallback = Some(self.host.clone());
                    }
                    self.host = next;
                    cx.notify();
                    return true;
                }
            }
            self.host_confirmed = true;
            self.sweep_fallback = None;
            self.sweep_settling = false;
        }
        self.model.set_rows(rows);
        self.open_review_on_boot(cx);
        cx.notify();
        false
    }

    /// Capture knob of the `COMET_OPEN_BOARD` family (gh#276): with
    /// `COMET_OPEN_REVIEW=1` the first board frame that carries a reviewable
    /// row opens its review.
    ///
    /// A design pass has to photograph this surface, and the review route is
    /// the one route with no deep link into it — it is reached by pressing `r`
    /// on a row, which is exactly what a capture script cannot rely on: a
    /// second app in front of this one swallows the key, and the pass then
    /// photographs the chat instead (the same failure gh#295 added
    /// `COMET_OPEN_BOARD` for).
    fn open_review_on_boot(&mut self, cx: &mut Context<Self>) {
        if self.review_on_boot_done || !std::env::var("COMET_OPEN_REVIEW").is_ok_and(|v| v == "1") {
            return;
        }
        // A row in `review` first — that is the state this window is about —
        // and any attempted row after it, so the knob still lands on a board
        // whose only attempt failed.
        let rows = self.model.rows.clone();
        let Some(row) = rows
            .iter()
            .find(|row| row.state == board::BoardState::Review.as_str() && board::reviewable(row))
            .or_else(|| rows.iter().find(|row| board::reviewable(row)))
        else {
            return;
        };
        self.review_on_boot_done = true;
        cx.emit(BoardEvent::OpenReview {
            task_id: row.id.clone(),
            chat_id: row.review_chat_id.clone().or_else(|| row.chat_id.clone()),
        });
    }

    /// The watch loop's verdict on a host whose stream just ended. Returns
    /// whether to try the next candidate immediately (no backoff — the sweep
    /// should not take one round-trip per device *plus* two seconds each).
    fn host_stream_ended(&mut self, delivered: bool, cx: &mut Context<Self>) -> bool {
        if delivered {
            // It does host the board; the stream dropped under it (engine
            // restart, relay blip). Stay put — the rows stay on screen under
            // the banner and the same host is re-subscribed after the backoff.
            self.error = Some("Board stream interrupted — retrying".into());
            cx.notify();
            return false;
        }
        self.host_confirmed = false;
        if self.host_pinned {
            let label = self.host_label(cx);
            self.error = Some(format!("{label} hosts no board").into());
            cx.notify();
            return false;
        }
        let (devices, local) = {
            let state = self.state.read(cx);
            (state.devices.clone(), state.local_device_id.clone())
        };
        let candidates = board::host_candidates(&devices, local.as_deref());
        match board::next_host_candidate(&candidates, self.host.as_deref()) {
            Some(next) => {
                self.host = next;
                self.error = None;
                // A settle pass that found its host silent is over; the sweep
                // is back to judging frames on their evidence.
                self.sweep_settling = false;
                cx.notify();
                true
            }
            None => {
                // Everyone has been asked. A board held for want of dispatch
                // evidence is the best answer there is — settle on it (gh#125).
                if let Some(fallback) = self.sweep_fallback.take() {
                    self.host = fallback;
                    self.sweep_settling = true;
                    self.error = None;
                    cx.notify();
                    return true;
                }
                // Nobody hosts a board at all. Start over from this device
                // after the backoff: the box may be booting, and a panel that
                // gave up would need a restart to notice.
                self.host = None;
                self.error =
                    Some("No device here hosts a board — open the host menu to pick one".into());
                cx.notify();
                false
            }
        }
    }

    // ---- watch lifecycle ----

    /// Start the `WatchBoard` subscription (idempotent). Retries with a flat
    /// 2 s delay if the stream fails or ends; the last rows stay visible under
    /// an error banner meanwhile.
    fn ensure_watch(&mut self, cx: &mut Context<Self>) {
        if self.started {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            // Engine still booting — retry on the next state change via the
            // observation in `new`.
            return;
        };
        self.started = true;
        self.watch_task = Some(Self::spawn_watch(engine, cx));
    }

    /// The watch loop. Each pass asks the panel which device to try, subscribes
    /// there, and pumps frames; a stream that ends hands the verdict back to
    /// [`BoardPanel::host_stream_ended`], which either advances the sweep (and
    /// we re-subscribe at once) or keeps the host and backs off 2 s.
    fn spawn_watch(engine: EngineHandle, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                // The sweep's current position, read fresh — the operator may
                // have pinned a device while the last stream was running.
                let Ok(params) =
                    this.update(cx, |panel, _| panel.host_params(serde_json::json!({})))
                else {
                    return;
                };
                let mut delivered = false;
                // The sweep held this host's answer and moved on (gh#125):
                // drop the stream and re-subscribe at the new candidate now.
                let mut advanced = false;
                match engine
                    .client()
                    .subscribe(methods::WATCH_BOARD, params)
                    .await
                {
                    Ok(mut rx) => {
                        while let Some(value) = rx.recv().await {
                            delivered = true;
                            let alive = this.update(cx, |panel, cx| {
                                match serde_json::from_value::<Vec<TaskRow>>(value) {
                                    Ok(rows) => panel.host_frame(rows, cx),
                                    Err(err) => {
                                        tracing::warn!(
                                            error = %err,
                                            "board: dropping malformed watch frame"
                                        );
                                        false
                                    }
                                }
                            });
                            match alive {
                                Ok(true) => {
                                    advanced = true;
                                    break;
                                }
                                Ok(false) => {}
                                Err(_) => return,
                            }
                        }
                        if advanced {
                            continue;
                        }
                    }
                    Err(err) => {
                        // The subscribe itself was refused — the transport is
                        // down, not the board. Say so and keep the host.
                        if this
                            .update(cx, |panel, cx| {
                                panel.error = Some(format!("Board unavailable: {err}").into());
                                cx.notify();
                            })
                            .is_err()
                        {
                            return;
                        }
                        cx.background_executor().timer(Duration::from_secs(2)).await;
                        continue;
                    }
                }
                match this.update(cx, |panel, cx| panel.host_stream_ended(delivered, cx)) {
                    // Next candidate: ask now, the sweep is already one
                    // round-trip per device.
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(_) => return,
                }
                cx.background_executor().timer(Duration::from_secs(2)).await;
            }
        })
    }

    // ---- verbs ----

    /// Release a ready task, asking which runtime first. The operator is
    /// dispatching, so there is no `via` — provenance is never fabricated (the
    /// same rule the TUI follows). The picker defaults to the route's runtime
    /// and loads the options off the engine; `enter` confirms, `esc` cancels.
    fn dispatch(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(engine) = self.engine(cx) else {
            self.set_notice("Engine not connected", cx);
            return;
        };
        let Some(row) = self.model.task(id) else {
            return;
        };
        if !row.dispatchable {
            self.set_notice(
                format!("{} has no route — it cannot be dispatched", row.identifier),
                cx,
            );
            return;
        }
        let task_id = row.id.clone();
        let identifier = row.identifier.clone();
        // A fresh picker starts with an empty model filter (the search input
        // persists across dispatches on the panel).
        self.dispatch_search
            .update(cx, |input, cx| input.set_text("", cx));
        self.dispatch = Some(DispatchDraft {
            route_runtime: row.runtime.clone(),
            route_account: row.account.clone(),
            task_id,
            identifier,
            runtimes: Vec::new(),
            active_runtime: 0,
            runtime_error: None,
            catalogs: HashMap::new(),
            row: PickerRow::Runtime,
            active_model: 0,
            accounts: AccountCatalog::Loading,
            active_account: 0,
            // The same email this dispatch will send as `viaUser` (gh#74) —
            // the picker warns about exactly the claim the box will record and
            // the guard will compare against, never a second answer to "who
            // are you" that could disagree with it.
            viewer: self
                .state
                .read(cx)
                .auth_user()
                .map(|user| user.email.clone())
                .filter(|email| !email.is_empty()),
        });
        cx.notify();
        self.load_dispatch_runtimes(engine.clone(), cx);
        self.load_dispatch_accounts(engine, cx);
    }

    /// Fetch `ListAgentAccounts` into the open picker (gh#74).
    ///
    /// The host's logins, not this laptop's: the run executes there, and a slot
    /// id only means anything on the device that saved it — the same reason the
    /// model catalog is fetched with the host passthrough. Loaded once per
    /// picker and filtered per runtime, since one call already returns every
    /// harness's slots.
    fn load_dispatch_accounts(&mut self, engine: EngineHandle, cx: &mut Context<Self>) {
        let Some(task_id) = self.dispatch.as_ref().map(|d| d.task_id.clone()) else {
            return;
        };
        let params = self.host_params(serde_json::json!({}));
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_AGENT_ACCOUNTS, params)
                .await;
            this.update(cx, |panel, cx| {
                // A newer pick (or a cancel) replaced this one mid-flight.
                let Some(draft) = panel.dispatch.as_mut() else {
                    return;
                };
                if draft.task_id != task_id {
                    return;
                }
                draft.accounts = match result {
                    Ok(value) => match serde_json::from_value::<AgentAccountsSnapshot>(value) {
                        Ok(snapshot) => AccountCatalog::Ready(snapshot.accounts),
                        Err(err) => AccountCatalog::Error(format!("Couldn't read accounts: {err}")),
                    },
                    Err(err) => AccountCatalog::Error(format!("Couldn't list accounts: {err}")),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Fetch `ListBoardRuntimes` into the open picker. The picker renders
    /// immediately with "Loading…"; the list landing re-homes the cursor onto
    /// the route's runtime and starts the model load for it. A failed load
    /// leaves the picker open (escape always works) with a notice naming the
    /// failure.
    fn load_dispatch_runtimes(&mut self, engine: EngineHandle, cx: &mut Context<Self>) {
        let Some(task_id) = self.dispatch.as_ref().map(|d| d.task_id.clone()) else {
            return;
        };
        // The host validates a dispatch's runtime override against its own
        // harness set, so the catalog has to come from there too.
        let params = self.host_params(serde_json::json!({}));
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_BOARD_RUNTIMES, params)
                .await;
            this.update(cx, |panel, cx| {
                // A newer pick (or a cancel) replaced this one while the load
                // was in flight — its options would be stale, so drop them.
                let Some(draft) = panel.dispatch.as_mut() else { return };
                if draft.task_id != task_id {
                    return;
                }
                match result {
                    Ok(value) => match serde_json::from_value::<Vec<BoardRuntime>>(value) {
                        Ok(options) => {
                            draft.runtimes = options;
                            draft.active_runtime = default_runtime_index(
                                &draft.runtimes,
                                draft.route_runtime.as_deref(),
                            );
                        }
                        Err(err) => {
                            draft.runtime_error = Some(format!("Couldn't read runtimes: {err}"));
                            panel.set_notice(
                                format!("Couldn't read runtimes — enter dispatches with the route's: {err}"),
                                cx,
                            );
                        }
                    },
                    Err(err) => {
                        draft.runtime_error = Some(format!("Couldn't list runtimes: {err}"));
                        panel.set_notice(
                            format!("Couldn't list runtimes — enter dispatches with the route's: {err}"),
                            cx,
                        );
                    }
                }
                // Whatever landed (or failed), start the model load for the
                // highlighted runtime — enter must know which model it will
                // release under, not just which runtime.
                panel.ensure_dispatch_models(engine.clone(), cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Kick off `ListModels` for the highlighted runtime when its catalog isn't
    /// loaded yet. Idempotent per runtime: a catalog that landed stays cached,
    /// so highlighting back and forth never re-fetches.
    fn ensure_dispatch_models(&mut self, engine: EngineHandle, cx: &mut Context<Self>) {
        let Some(draft) = self.dispatch.as_mut() else {
            return;
        };
        let Some(runtime) = draft.active_runtime_name() else {
            return;
        };
        let slot = draft.catalogs.entry(runtime.clone()).or_default();
        if !matches!(slot, ModelCatalog::Idle) {
            return;
        }
        *slot = ModelCatalog::Loading;
        let task_id = draft.task_id.clone();
        let runtime_for_landing = runtime.clone();
        // The run happens on the host device, so its harness is the one whose
        // model catalog a dispatch can pick from.
        let params = host_params(
            self.host.as_deref(),
            serde_json::json!({ "harness": runtime }),
        );
        cx.spawn(async move |this, cx| {
            // The canonical runtime name IS the harness's kebab-case id, so it
            // deserializes straight into `ListModels { harness }`.
            let result = engine.client().call(methods::LIST_MODELS, params).await;
            this.update(cx, |panel, cx| {
                // A newer pick (or a cancel) replaced this one mid-flight.
                let Some(draft) = panel.dispatch.as_mut() else {
                    return;
                };
                if draft.task_id != task_id {
                    return;
                }
                let loaded = match result {
                    Ok(value) => match serde_json::from_value::<Vec<BoardModelInfo>>(value) {
                        Ok(models) => ModelCatalog::Ready(models),
                        Err(err) => ModelCatalog::Error(format!("Couldn't read models: {err}")),
                    },
                    Err(err) => ModelCatalog::Error(format!("Couldn't list models: {err}")),
                };
                draft.catalogs.insert(runtime_for_landing.clone(), loaded);
                // If the operator moved off this runtime while loading, the
                // landing must not re-home the highlight it left behind.
                if draft
                    .runtimes
                    .get(draft.active_runtime)
                    .map(|r| r.name.as_str())
                    == Some(runtime_for_landing.as_str())
                {
                    draft.active_model = 0;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// `enter` on the picker: dispatch with the highlighted runtime and model.
    /// The model highlight indexes the FILTERED list — typing narrows the
    /// rows, and enter dispatches the highlighted match.
    fn confirm_dispatch(&mut self, cx: &mut Context<Self>) {
        let indices = self.dispatch_filtered_models(cx);
        let Some(draft) = self.dispatch.take() else {
            return;
        };
        let catalog_ix = indices.get(draft.active_model).copied();
        let choice = draft.choice(catalog_ix);
        let billed_to = draft.billed_to();
        self.send_dispatch(&draft.task_id, &draft.identifier, choice, billed_to, cx);
    }

    /// The catalog rows (indices) the dispatch picker displays for the active
    /// runtime, filtered by the model query.
    fn dispatch_filtered_models(&self, cx: &App) -> Vec<usize> {
        let Some(draft) = self.dispatch.as_ref() else {
            return Vec::new();
        };
        let Some(ModelCatalog::Ready(models)) = draft.catalog() else {
            return Vec::new();
        };
        let query = self.dispatch_search.read(cx).text().to_string();
        filtered_model_indices(models, &query)
    }

    /// Focus the picker's model search input (typing then lands there). Only
    /// meaningful while the picker is open.
    fn focus_dispatch_model_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.dispatch_search.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    }

    /// Clicking a runtime chip: select that runtime and load its models. A
    /// selection is never itself a release — the model row (or enter) is.
    fn select_runtime(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = self.dispatch.as_mut() else {
            return;
        };
        let Some(ix) = draft.runtimes.iter().position(|r| r.name == name) else {
            return;
        };
        if draft.active_runtime != ix {
            draft.active_runtime = ix;
            // A new harness's highlight starts on its default — the old pick
            // would be a model the new harness may not even offer, and the old
            // login one it cannot spend at all.
            draft.active_model = 0;
            draft.active_account = 0;
            // The runtime is settled; the next input belongs to the models.
            draft.row = PickerRow::Model;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.ensure_dispatch_models(engine, cx);
        // Typing after a click lands in the model filter, not nowhere; a query
        // that narrowed the old harness's catalog gets dropped for the new one.
        self.dispatch_search
            .update(cx, |input, cx| input.set_text("", cx));
        self.focus_dispatch_model_search(window, cx);
        cx.notify();
    }

    /// Clicking an account chip: spend that login. A selection is never itself
    /// a release — the model row (or enter) is, exactly as for the runtime
    /// strip. Whose limits a run burns is too consequential to be one click
    /// away from happening by accident.
    fn select_account(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(draft) = self.dispatch.as_mut() else {
            return;
        };
        if ix > draft.account_options().len() {
            return;
        }
        draft.active_account = ix;
        draft.row = PickerRow::Account;
        cx.notify();
    }

    /// Clicking a model row: dispatch with the current runtime + that model.
    /// `catalog_ix` is the catalog row's own index (rows carry it through the
    /// filtered display).
    fn confirm_with_model(&mut self, catalog_ix: usize, cx: &mut Context<Self>) {
        let Some(draft) = self.dispatch.take() else {
            return;
        };
        let choice = draft.choice(Some(catalog_ix));
        let billed_to = draft.billed_to();
        self.send_dispatch(&draft.task_id, &draft.identifier, choice, billed_to, cx);
    }

    /// Send the release the host refused, this time naming who it charges
    /// (gh#101) — the panel's `--bill`.
    fn confirm_bill(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_bill.take() else {
            return;
        };
        let choice = DispatchChoice {
            bill: Some(pending.billed_to.clone()),
            ..pending.choice
        };
        self.send_dispatch(
            &pending.task_id,
            &pending.identifier,
            choice,
            Some(pending.billed_to),
            cx,
        );
    }

    /// Send `DispatchTask` for a task, with the picked runtime/model/account
    /// overrides (if any). The confirmation names the chat AND the model the run
    /// will actually use — the override when one was picked, else the harness
    /// default the catalog leads with — plus the account, when the dispatch
    /// chose one over the route's.
    ///
    /// Every call also carries who released it (gh#74): this device's id and the
    /// signed-in user, so a teammate's dispatch is recorded as theirs instead of
    /// as an anonymous operator. Hints, not credentials — the box cannot check
    /// either, and nothing is authorized on them.
    fn send_dispatch(
        &mut self,
        task_id: &str,
        identifier: &str,
        choice: DispatchChoice,
        // Who this release charges, as the picker resolved it — carried so a
        // `require-own` refusal can be turned into a confirm that names them
        // (gh#101). `None` where the panel could not tell, and then a refusal
        // is just an error: offering "bill them anyway" without being able to
        // say who "them" is would be the opposite of this feature.
        billed_to: Option<String>,
        cx: &mut Context<Self>,
    ) {
        // A new release supersedes any confirm still on offer.
        self.pending_bill = None;
        let Some(engine) = self.engine(cx) else {
            self.set_notice("Engine not connected", cx);
            return;
        };
        let state = self.state.read(cx);
        let via_device = state.local_device_id.clone();
        // Email when the profile carries one — a slot id or a WorkOS id is not
        // what a person reading the attempt row is looking for.
        let via_user = state
            .auth_user()
            .map(|user| user.email.clone())
            .filter(|email| !email.is_empty());
        // A retry on a blocked row ends the live attempt it is stuck on before
        // releasing (gh#49). Read the row now, not when the picker opened: if
        // it cleared (cancelled, or the agent moved on) meanwhile, a plain
        // dispatch is the right call and the engine's one-live-attempt guard
        // is free to refuse if it did not.
        let replace = self
            .model
            .task(task_id)
            .is_some_and(|row| row.state() == BoardState::Blocked);
        let task_id = task_id.to_string();
        let identifier = identifier.to_string();
        let DispatchChoice {
            runtime,
            model,
            effective_model,
            account,
            bill,
        } = choice.clone();
        // The dispatch runs where the board is: the worktree, the chat and the
        // agent all land on the host device.
        let host = self.host.clone();
        // Computed before the params consume the overrides.
        let detail = match (&runtime, &effective_model) {
            (Some(runtime), Some(model)) => format!(" · {runtime} / {model}"),
            (Some(runtime), None) => format!(" · {runtime}"),
            _ => String::new(),
        };
        // The account only when the dispatch chose one: the route's is what the
        // board would have spent anyway, and naming it every time would make
        // the exception invisible.
        let detail = match &account {
            Some(account) => format!("{detail} · on {}", account.label),
            None => detail,
        };
        let account_id = account.map(|a| a.id);
        cx.spawn(async move |this, cx| {
            let mut params = host_params(host.as_deref(), serde_json::json!({ "taskId": task_id }));
            if replace {
                params["replace"] = serde_json::Value::Bool(true);
            }
            if let (Some(runtime), Some(object)) = (runtime, params.as_object_mut()) {
                object.insert("runtime".into(), serde_json::Value::String(runtime));
            }
            if let (Some(model), Some(object)) = (model, params.as_object_mut()) {
                object.insert("model".into(), serde_json::Value::String(model));
            }
            if let (Some(account), Some(object)) = (account_id, params.as_object_mut()) {
                object.insert("account".into(), serde_json::Value::String(account));
            }
            if let (Some(bill), Some(object)) = (bill, params.as_object_mut()) {
                object.insert("bill".into(), serde_json::Value::String(bill));
            }
            if let (Some(device), Some(object)) = (via_device, params.as_object_mut()) {
                object.insert("viaDevice".into(), serde_json::Value::String(device));
            }
            if let (Some(user), Some(object)) = (via_user, params.as_object_mut()) {
                object.insert("viaUser".into(), serde_json::Value::String(user));
            }
            let result = engine.client().call(methods::DISPATCH_TASK, params).await;
            this.update(cx, |panel, cx| match result {
                Ok(value) => {
                    let chat = value
                        .get("chatId")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .unwrap_or_default();
                    panel.set_notice(
                        format!("Dispatched {identifier} — chat {chat} is on it{detail}"),
                        cx,
                    );
                }
                Err(err) => {
                    let message = err.to_string();
                    // The host refuses a cross-billed release under
                    // `require-own` (gh#101). Turn that into the confirm the
                    // CLI spells `--bill`, rather than a dead end whose only
                    // remedy is editing someone else's routing.toml.
                    match billed_to.filter(|_| message.contains(board::REQUIRE_OWN_REFUSAL)) {
                        Some(billed_to) => {
                            panel.set_notice(
                                format!(
                                    "{identifier} would spend {billed_to}'s subscription. \
                                     Press enter to dispatch it on their plan, esc to back out"
                                ),
                                cx,
                            );
                            panel.pending_bill = Some(PendingBill {
                                task_id: task_id.clone(),
                                identifier: identifier.clone(),
                                choice,
                                billed_to,
                            });
                        }
                        None => {
                            panel.set_notice(
                                format!("Couldn't dispatch {identifier}: {message}"),
                                cx,
                            );
                        }
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// End a running task's live attempt (interrupt + archive the chat). The
    /// issue stays open: cancel ends attempts, never tasks.
    fn cancel(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(engine) = self.engine(cx) else {
            self.set_notice("Engine not connected", cx);
            return;
        };
        let Some(row) = self.model.task(id) else {
            return;
        };
        let identifier = row.identifier.clone();
        let task_id = row.id.clone();
        let params = self.host_params(serde_json::json!({ "taskId": task_id }));
        cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::CANCEL_TASK, params).await;
            this.update(cx, |panel, cx| match result {
                Ok(_) => panel.set_notice(format!("Cancelled {identifier}"), cx),
                Err(err) => panel.set_notice(format!("Couldn't cancel {identifier}: {err}"), cx),
            })
            .ok();
        })
        .detach();
    }

    /// `r`: open the selected row's review (gh#180).
    ///
    /// Offered on any row that has been attempted at all, not only on `review`
    /// rows: a failed attempt and a cancelled one both left a branch and a run
    /// journal behind them, and "what did it actually change before it gave up"
    /// is the same question this screen answers for a finished one. A row
    /// nothing has ever run on has no attempt to review, and says so.
    fn open_review(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(row) = self.model.task(id) else {
            return;
        };
        if !board::reviewable(row) {
            self.set_notice(
                format!(
                    "{} has never been dispatched — nothing to review",
                    row.identifier
                ),
                cx,
            );
            return;
        }
        cx.emit(BoardEvent::OpenReview {
            task_id: row.id.clone(),
            chat_id: row.review_chat_id.clone().or_else(|| row.chat_id.clone()),
        });
    }

    /// Jump to a running task's chat (herdr-board's `g`): the attempt's chat is
    /// where the work is, and comet's answer to a pane is a chat. The board
    /// gives way — the chat is the destination.
    fn open_chat(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.model.task(id) else {
            return;
        };
        let Some(chat_id) = row.chat_id.clone() else {
            self.set_notice(
                format!("{} is running but has no chat to open", row.identifier),
                cx,
            );
            return;
        };
        self.state
            .update(cx, |s, cx| s.select_chat(Some(chat_id), cx));
        window.dispatch_action(Box::new(ToggleBoard), cx);
    }

    // ---- the peek panel (gh#132) ----

    /// Open the peek on whatever the cursor is on, and fetch its issue text.
    ///
    /// Idempotent: opening an already-open peek only re-points it, which is
    /// what a click on a second row does.
    fn open_peek(&mut self, cx: &mut Context<Self>) {
        if self.model.selected_task().is_none() {
            // A section or group header is not a door. Silently, because the
            // click that landed here has already folded it.
            return;
        }
        self.peek = true;
        self.ensure_detail(cx);
    }

    /// `space`: open the peek on the selected row, or shut it.
    fn toggle_peek(&mut self, cx: &mut Context<Self>) {
        if self.peek {
            self.peek = false;
            cx.notify();
        } else {
            self.open_peek(cx);
            cx.notify();
        }
    }

    /// Fetch the selected row's issue text, unless it is already held or
    /// already in flight.
    ///
    /// Called from every path that can change what the open peek is pointed at
    /// — opening it, and moving the cursor while it is open — because the peek
    /// follows the selection and a body from the previous row is worse than a
    /// blank one.
    fn ensure_detail(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.model.selected_task().map(|row| row.id.clone()) else {
            return;
        };
        if self.detail.as_ref().is_some_and(|d| d.id == id)
            || self.detail_pending.as_deref() == Some(id.as_str())
        {
            return;
        }
        // A new row: whatever is on screen belongs to the old one.
        self.detail = None;
        self.detail_error = None;
        let Some(engine) = self.engine(cx) else {
            self.detail_error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        self.detail_pending = Some(id.clone());
        let params = self.host_params(serde_json::json!({ "taskId": id }));
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call_as::<TaskDetail>(methods::READ_BOARD_TASK, params)
                .await;
            this.update(cx, |panel, cx| {
                // A reply for a row the cursor has already left is stale by
                // definition — the newer fetch owns the slot.
                if panel.detail_pending.as_deref() != Some(id.as_str()) {
                    return;
                }
                panel.detail_pending = None;
                match result {
                    Ok(detail) => panel.detail = Some(detail),
                    Err(err) => panel.detail_error = Some(format!("{err}").into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// `enter` on the board: fold a header, or run the selected row's primary
    /// verb — dispatch a ready task, retry a failed one (gh#42), open a running
    /// or blocked one's chat, open a review's PR.
    ///
    /// Which verb that is comes from [`board::primary_action`] rather than a
    /// second match here (gh#176), so the chip the row wears and the key the
    /// operator presses cannot come to mean different things. A blocked row is
    /// the case that earns the shared rule: its actions lead with Retry, but
    /// enter opens the chat (gh#49) — its agent is alive and awaiting input, so
    /// the chat is where the answer is, and ending the attempt to replace it is
    /// explicit enough for a chip.
    fn activate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = self.model.on_section() {
            self.model.toggle_collapsed(state);
            cx.notify();
            return;
        }
        if let Some((state, route)) = self.model.on_group() {
            self.model.toggle_group(state, route.as_deref());
            cx.notify();
            return;
        }
        let Some(row) = self.model.selected_task() else {
            return;
        };
        let id = row.id.clone();
        if let Some(action) = board::primary_action(row) {
            self.run_action(&id, action, window, cx);
        }
    }

    // ---- find field ----

    fn open_find_field(&mut self, cx: &mut Context<Self>) {
        self.model.open_find();
        if self.find.is_none() {
            let input =
                cx.new(|cx| ComposerInput::with_context("Search the board…", "PaletteSearch", cx));
            let events = cx.subscribe(&input, |this: &mut Self, _, event, cx| {
                if matches!(event, ComposerInputEvent::Edited(_))
                    && let Some(q) = this.find.as_ref().map(|f| f.read(cx).text().to_string())
                {
                    this.model.filter = Filter::Text(q);
                    this.model.typing = true;
                    this.model.clamp_selection();
                    cx.notify();
                }
            });
            self.find = Some(input);
            self._find_events = Some(events);
        } else if let Some(find) = &self.find {
            find.update(cx, |input, cx| input.set_text("", cx));
        }
        self.find_focus_pending = true;
        cx.notify();
    }

    fn accept_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.model.accept_find();
        // The find input unmounts with the field; hand keyboard focus back to
        // the board so ↑↓/f/… keep working with no extra click.
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn escape_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.model.escape_find();
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    // ---- key handling ----

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let mods = ks.modifiers;
        let key = ks.key.as_str();

        if self.model.typing {
            // The focused find field owns the letters; only the navigation
            // keys bubble here (PaletteSearch leaves them unbound).
            match key {
                "enter" => {
                    self.accept_find(window, cx);
                    cx.stop_propagation();
                }
                "escape" => {
                    self.escape_find(window, cx);
                    cx.stop_propagation();
                }
                "up" | "down" => {
                    let delta = if key == "up" { -1 } else { 1 };
                    self.move_selection(delta, cx);
                    cx.stop_propagation();
                }
                _ => {}
            }
            return;
        }

        // The dispatch picker owns the keys while it is open: up/down switches
        // between the runtime and model rows — or, with the model filter
        // focused, walks the filtered matches — left/right moves the highlight
        // within the focused row, typing filters the models, enter dispatches
        // with both highlights, escape cancels.
        if self.dispatch.is_some() {
            let search_focused = self
                .dispatch_search
                .read(cx)
                .focus_handle(cx)
                .is_focused(window);
            match key {
                "up" | "down" => {
                    let delta = if key == "up" { -1 } else { 1 };
                    let filter_empty = self.dispatch_search.read(cx).is_empty();
                    let model_count = self.dispatch_filtered_models(cx).len();
                    let mut focus_search = false;
                    let mut focus_frame = false;
                    if let Some(draft) = self.dispatch.as_mut() {
                        match draft.row {
                            // The top row: only down leads anywhere.
                            PickerRow::Runtime => {
                                if delta > 0 {
                                    draft.row = PickerRow::Account;
                                    focus_frame = true;
                                }
                            }
                            PickerRow::Account if delta > 0 => {
                                draft.row = PickerRow::Model;
                                draft.active_model = 0;
                                focus_search = true;
                            }
                            PickerRow::Account => {
                                draft.row = PickerRow::Runtime;
                                focus_frame = true;
                            }
                            PickerRow::Model if search_focused => {
                                // Empty filter + up on the top row returns to
                                // the account strip; otherwise walk the matches.
                                if filter_empty && delta < 0 && draft.active_model == 0 {
                                    draft.row = PickerRow::Account;
                                    focus_frame = true;
                                } else {
                                    draft.active_model = popover::menu_step(
                                        Some(draft.active_model),
                                        model_count,
                                        delta,
                                    )
                                    .unwrap_or(0);
                                }
                            }
                            PickerRow::Model => {
                                draft.row = PickerRow::Account;
                                focus_frame = true;
                            }
                        }
                    }
                    if focus_search {
                        self.focus_dispatch_model_search(window, cx);
                    } else if focus_frame {
                        window.focus(&self.focus_handle, cx);
                    }
                    self.reveal_model_chip();
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                "left" | "right" => {
                    let delta = if key == "left" { -1 } else { 1 };
                    let model_count = self.dispatch_filtered_models(cx).len();
                    let mut runtime_moved = false;
                    if let Some(draft) = self.dispatch.as_mut() {
                        match draft.row {
                            PickerRow::Runtime if !draft.runtimes.is_empty() => {
                                draft.active_runtime = (draft.active_runtime as isize + delta)
                                    .clamp(0, draft.runtimes.len() as isize - 1)
                                    as usize;
                                draft.active_model = 0;
                                // Another harness is another set of logins —
                                // the slot highlighted for the old one may not
                                // even be offered here.
                                draft.active_account = 0;
                                runtime_moved = true;
                            }
                            PickerRow::Account => {
                                // Row 0 is the route's own account, so the walk
                                // is one longer than the offered logins.
                                let count = draft.account_options().len() + 1;
                                draft.active_account = (draft.active_account as isize + delta)
                                    .clamp(0, count as isize - 1)
                                    as usize;
                            }
                            PickerRow::Model => {
                                draft.active_model = popover::menu_step(
                                    Some(draft.active_model),
                                    model_count,
                                    delta,
                                )
                                .unwrap_or(0);
                            }
                            _ => {}
                        }
                    }
                    // A runtime move re-homes onto the default AND drops the
                    // model filter — the query that narrowed the old harness's
                    // catalog would otherwise read as "no matches" on the new.
                    if runtime_moved {
                        self.dispatch_search
                            .update(cx, |input, cx| input.set_text("", cx));
                    }
                    // Either way keep the highlight in view.
                    self.reveal_model_chip();
                    if let Some(engine) = self.engine(cx) {
                        self.ensure_dispatch_models(engine, cx);
                    }
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                "enter" => {
                    self.confirm_dispatch(cx);
                    // The picker closes on dispatch; hand keyboard control back
                    // to the board (the model filter had been focused).
                    if self.dispatch.is_none() {
                        window.focus(&self.focus_handle, cx);
                    }
                    cx.stop_propagation();
                    return;
                }
                "escape" => {
                    self.dispatch = None;
                    self.dispatch_search
                        .update(cx, |input, cx| input.set_text("", cx));
                    window.focus(&self.focus_handle, cx);
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                _ => {
                    // The model search input owns plain printable keys while
                    // it is focused (PaletteSearch binds only text editing): a
                    // letter like `f` must not fall through to the frame
                    // handler below and cycle the board filter WHILE it also
                    // types.
                    if dispatch_picker_owns_key(key, search_focused) {
                        cx.stop_propagation();
                        return;
                    }
                }
            }
        }

        // A release the host refused because it charges somebody else owns the
        // next enter (gh#101). Ahead of the board's own keys, and only while
        // the offer stands: this IS the confirm dialog, and a confirm that the
        // cursor could walk away from without answering is not one.
        if self.pending_bill.is_some() {
            match key {
                "enter" => {
                    self.confirm_bill(cx);
                    cx.stop_propagation();
                    return;
                }
                "escape" => {
                    self.pending_bill = None;
                    self.set_notice("Left it unreleased", cx);
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }

        match key {
            "up" | "down" => {
                let delta = if key == "up" { -1 } else { 1 };
                self.move_selection(delta, cx);
                cx.stop_propagation();
            }
            "enter" => {
                self.activate(window, cx);
                cx.stop_propagation();
            }
            // Open the selected row for reading, or shut it again (gh#132).
            // `space` because `enter` is the release and must stay one keypress
            // from the list: inspecting a row and dispatching it are different
            // enough that they should never be the same key.
            "space" => {
                self.toggle_peek(cx);
                cx.stop_propagation();
            }
            // Escape shuts the peek before it shuts the board: the last thing
            // opened is the first thing closed. Only while it is actually
            // drawn, though — on a header the peek renders nothing, and an
            // escape that swallowed itself against invisible state would read
            // as the key not working.
            "escape" if self.peek && self.model.selected_task().is_some() => {
                self.peek = false;
                cx.notify();
                cx.stop_propagation();
            }
            "escape" => {
                window.dispatch_action(Box::new(ToggleBoard), cx);
                cx.stop_propagation();
            }
            // `/` needs no shift check — it is itself a shifted key on US
            // layouts, and there is no unshifted meaning to preserve.
            "/" => {
                self.open_find_field(cx);
                cx.stop_propagation();
            }
            "f" if !mods.modified() => {
                if let Some(message) = self.model.cycle_filter() {
                    self.set_notice(message, cx);
                }
                cx.notify();
                cx.stop_propagation();
            }
            "f" if mods.shift => {
                self.model.clear_filter();
                cx.notify();
                cx.stop_propagation();
            }
            // `r`: review what the selected row's attempt actually changed
            // (gh#180). A route change and not a board verb, so it leaves by
            // event — see [`BoardEvent`].
            "r" if !mods.modified() => {
                if let Some(id) = self.model.selected_task().map(|row| row.id.clone()) {
                    self.open_review(&id, cx);
                }
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let before = self.model.selected.clone();
        self.model.select_delta(delta);
        if self.model.selected != before {
            // Keep the cursor's line visible as it walks the board.
            let selected = self.model.selected.clone();
            if let Some(ix) = self
                .model
                .lines()
                .iter()
                .position(|line| line.id() == selected.as_deref().unwrap_or(""))
            {
                self.scroll.scroll_to_item(ix);
            }
            // An open peek follows the cursor (gh#132) — otherwise walking the
            // board would leave it showing a row you have left.
            if self.peek {
                self.ensure_detail(cx);
            }
            cx.notify();
        }
    }

    /// Scroll the filtered model list so the highlighted row stays in view.
    /// The list scrolls vertically (opencode's catalog is 70+ models), so the
    /// keyboard must reveal what it highlights — the rows are the scroll
    /// container's direct children, so the display index maps 1:1.
    fn reveal_model_chip(&self) {
        if let Some(draft) = self.dispatch.as_ref() {
            let model_scroll = self.model_scroll.clone();
            model_scroll.scroll_to_item(draft.active_model);
        }
    }

    /// A transient footer message, cleared after a beat.
    fn set_notice(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        let text = text.into();
        self.notice = Some(text.clone());
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(4)).await;
            this.update(cx, |panel, cx| {
                if panel.notice.as_ref() == Some(&text) {
                    panel.notice = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    // ---- rendering ----

    /// The panel's name, and the two controls the canvas gives it (E2).
    ///
    /// Left to right: "Board", the device serving it, that device's presence
    /// dot, then — hard right — the route chip and the find button. gh#295 took
    /// three things off it that the canvas does not have: a count badge (the
    /// counts belong to the group headers, where they say which twelve), a
    /// "Filter" control (the route chip IS the filter, and says what it is
    /// filtered to rather than that filtering exists), and a close ✕ (the
    /// titlebar's board toggle is the dock's switch, and `esc` still shuts it).
    fn render_header(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let typing = self.model.typing;

        let mut header = div()
            .h(px(40.0))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .px(px(PAD_X))
            .border_b_1()
            .border_color(theme.border)
            .child(
                icon(icons::CHECKLIST)
                    .size(px(15.0))
                    .text_color(theme.text_muted),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(Theme::TEXT_BODY))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(SharedString::from("Board")),
            )
            // Whose board this is belongs in the panel's name (gh#125), and the
            // canvas names it on every board rather than only on an install
            // with two devices: the rows mean something different by which
            // box produced them, and a header that is silent about it on the
            // ordinary install is silent exactly where the habit forms.
            .child(self.render_host_title(&theme, cx))
            .child(div().flex_1());

        if typing {
            // The `/` field: the query input inline, then a close chip.
            let find_input = self.find.clone().expect("find field open implies an input");
            header = header.child(
                div()
                    .flex_1()
                    .max_w(px(240.0))
                    .h(px(26.0))
                    .px(px(8.0))
                    .rounded(px(Theme::RADIUS_CHIP))
                    .border_1()
                    .border_color(theme.border_strong)
                    .bg(theme.wash(0.05))
                    .flex()
                    .items_center()
                    .child(find_input),
            );
            header = header.child(
                div()
                    .id("board-find-close")
                    .size(px(24.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(Theme::RADIUS_CHIP))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.wash(0.1)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        // Close the field, keeping the query — the X is the
                        // mouse's "enter" (esc still clears).
                        this.accept_find(window, cx);
                    }))
                    .child(
                        icon(icons::CLOSE)
                            .size(px(13.0))
                            .text_color(theme.text_muted),
                    ),
            );
        } else {
            // The Automations indicator (gh#490): quiet until opened, tinted
            // when a rule is unhealthy. Before the route chip so the header's
            // rightmost pair stays filter-then-find.
            let automations_chip = self.render_automations_chip(&theme, cx);
            header = header.child(automations_chip);
            // The route chip (E3): the word "route" in `--subtle`, then what
            // the board is showing in `--text`. It is the filter — clicking it
            // steps the routes exactly as `f` does — said as the state it puts
            // the board in rather than as the verb that changes it, which is
            // why it can be there when nothing is filtered ("route all") and
            // still be worth reading.
            let (word, value): (&str, SharedString) = match &self.model.filter {
                Filter::All => ("route", "all".into()),
                Filter::Route(route) => ("route", route.clone().into()),
                Filter::NoRoute => ("route", board::NO_ROUTE.into()),
                // A typed query is the other question (`/`), and the chip says
                // which one it is holding rather than calling a query a route.
                Filter::Text(query) => ("find", query.clone().into()),
            };
            header = header.child(
                div()
                    .id("board-route")
                    .h(px(24.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(Theme::SPACE_SM))
                    .rounded(px(Theme::RADIUS_CHIP))
                    .cursor_pointer()
                    .bg(motion::hover_blend(
                        "board-route",
                        theme.chip,
                        theme.wash(0.14),
                    ))
                    .on_hover(motion::hover_listener("board-route"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(message) = this.model.cycle_filter() {
                            this.set_notice(message, cx);
                        }
                        cx.notify();
                    }))
                    .text_size(px(Theme::TEXT_DENSE))
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme.text_subtle)
                            .child(SharedString::from(word)),
                    )
                    .child(
                        div()
                            .max_w(px(140.0))
                            .truncate()
                            .text_color(theme.text)
                            .child(value),
                    ),
            );
            // `/` find.
            header = header.child(
                div()
                    .id("board-find")
                    .size(px(24.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(Theme::RADIUS_CHIP))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.wash(0.1)))
                    .on_click(cx.listener(|this, _, _, cx| this.open_find_field(cx)))
                    .child(
                        icon(icons::MAGNIFER)
                            .size(px(14.0))
                            .text_color(theme.text_muted),
                    ),
            );
        }

        header.into_any_element()
    }

    /// The title's host segment: "on Tokenmaxxer9000", with the presence dot,
    /// as part of the panel's name (gh#125 — formerly a corner chip, gh#55).
    /// Clicking it opens the host menu. The dot is lit once that device has
    /// actually delivered rows the sweep settled on — while the sweep is still
    /// asking, the segment says who it is asking.
    fn render_host_title(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let (mut devices, local_id) = {
            let state = self.state.read(cx);
            (state.devices.clone(), state.local_device_id.clone())
        };
        // Registration order, matching the sweep and the settings switcher, so
        // rows never reshuffle on a heartbeat.
        devices.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let label = self.host_label(cx);
        let open = self.host_menu_open;
        let confirmed = self.host_confirmed;
        let pinned = self.host_pinned;
        let host = self.host.clone();
        // Online: the settled hue, the same green the Devices page paints.
        let emerald = theme.settled;

        // The device's name at 12px `--subtle` and its 5px dot, in the header's
        // own gap (E2). The canvas drops the word "on": a name beside "Board"
        // with a presence dot after it is already a sentence, and the preposition
        // was the only part of it that could not be seen at a glance.
        let mut chip = div()
            .id("board-host")
            .h(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .px(px(4.0))
            .rounded(px(Theme::RADIUS_CHIP))
            .cursor_pointer()
            .bg(if open {
                theme.wash(0.14)
            } else {
                theme.wash(0.0)
            })
            .when(!open, |el| el.hover(|s| s.bg(theme.wash(0.1))))
            .on_click(cx.listener(|this, _, _, cx| {
                let just_dismissed = this
                    .host_menu_dismissed_at
                    .is_some_and(|at| at.elapsed() < Duration::from_millis(400));
                this.host_menu_open = !this.host_menu_open && !just_dismissed;
                this.host_menu_dismissed_at = None;
                cx.notify();
            }))
            .text_size(px(Theme::TEXT_DENSE))
            .child(
                div()
                    .max_w(px(140.0))
                    .truncate()
                    .text_color(theme.text_subtle)
                    .child(label),
            )
            .child(
                div()
                    .size(px(5.0))
                    .flex_none()
                    // round-ok: status dot
                    .rounded_full()
                    // Lit once that device has delivered rows the sweep settled
                    // on; while the sweep is still asking, the dot is as quiet
                    // as the answer is.
                    .bg(if confirmed { emerald } else { theme.text_faint }),
            );

        if open {
            let menu = popover::popover_card(theme)
                .w(px(220.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.host_menu_open = false;
                    this.host_menu_dismissed_at = Some(std::time::Instant::now());
                    cx.notify();
                }))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(popover::menu_heading(theme, "Board host"))
                // Hand the sweep back: the panel finds whichever device hosts
                // a board, which is the right answer on almost every install.
                .child(
                    popover::menu_row(theme, !pinned, "board-host-auto")
                        .id("board-host-auto")
                        .on_click(cx.listener(|this, _, _, cx| this.set_host(None, false, cx)))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(SharedString::from("Automatic")),
                        )
                        .when(!pinned, |el| el.child(popover::menu_check(theme))),
                )
                .children(devices.into_iter().enumerate().map(|(ix, d)| {
                    let is_local = local_id.as_deref() == Some(d.id.as_str());
                    // "This device" is the absent passthrough, so a pinned
                    // local host and an unpinned one address the same engine.
                    let target = (!is_local).then(|| d.id.clone());
                    let is_active = pinned && host == target;
                    let name: SharedString = d.name.clone().into();
                    popover::menu_row(theme, is_active, format!("board-host-row-{ix}"))
                        .id(("board-host-row", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_host(target.clone(), true, cx);
                        }))
                        .child(div().flex_1().min_w_0().truncate().child(name.clone()))
                        .when(is_local, |el| {
                            el.child(
                                div()
                                    .flex_none()
                                    .text_size(px(Theme::TEXT_CAPTION))
                                    .text_color(theme.text_subtle)
                                    .child(SharedString::from("You")),
                            )
                        })
                        .when(is_active, |el| el.child(popover::menu_check(theme)))
                }))
                .into_any_element();
            chip = chip.child(popover::anchored_menu("board-host-menu", menu));
        }
        chip.into_any_element()
    }

    // ---- the Automations indicator (gh#490) ----

    /// Read the rules' health and history off the board host. Called when the
    /// popover opens and after a pause/resume, never on a timer: the popover
    /// is a glance, not a stream.
    fn fetch_automations(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let params = self.host_params(serde_json::json!({}));
        self.automations_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::READ_BOARD_AUTOMATIONS, params)
                .await;
            this.update(cx, |panel, cx| {
                match result {
                    Ok(value) => {
                        if let Ok(view) = serde_json::from_value::<AutomationsView>(value) {
                            panel.automations = Some(view);
                        }
                    }
                    // A host with no board method (an older box) or no board
                    // at all: the popover says the rules could not be read.
                    Err(err) => panel.notice = Some(format!("{err}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Pause or resume one rule from the popover — `enabled` through the same
    /// validating writer the settings page uses, so a resume the config does
    /// not support (no owner, no labels) is refused with the writer's words.
    fn set_automation_enabled(&mut self, rule: String, enabled: bool, cx: &mut Context<Self>) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let params = self.host_params(serde_json::json!({
            "op": "automation",
            "automation": rule,
            "key": "enabled",
            "value": if enabled { "true" } else { "false" },
        }));
        self.automations_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::WRITE_BOARD_CONFIG, params)
                .await;
            this.update(cx, |panel, cx| {
                if let Err(err) = result {
                    panel.notice = Some(format!("{err}").into());
                    cx.notify();
                } else {
                    panel.fetch_automations(cx);
                }
            })
            .ok();
        }));
    }

    /// The compact indicator: a wand at 24px, with the popover under it. The
    /// popover is the operational surface — health counts, the next
    /// reconciliation, the latest action, per-rule pause/resume — and the full
    /// editor deliberately is not (§gh#490): it deep-links to Settings.
    fn render_automations_chip(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let open = self.automations_open;
        let unhealthy = self
            .automations
            .as_ref()
            .is_some_and(|v| v.unhealthy_count() > 0);
        let mut chip = div()
            .id("board-automations")
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(Theme::RADIUS_CHIP))
            .cursor_pointer()
            .bg(if open { theme.wash(0.14) } else { theme.wash(0.0) })
            .when(!open, |el| el.hover(|s| s.bg(theme.wash(0.1))))
            .on_click(cx.listener(|this, _, _, cx| {
                let just_dismissed = this
                    .automations_dismissed_at
                    .is_some_and(|at| at.elapsed() < Duration::from_millis(400));
                this.automations_open = !this.automations_open && !just_dismissed;
                this.automations_dismissed_at = None;
                if this.automations_open {
                    this.fetch_automations(cx);
                }
                cx.notify();
            }))
            .child(icon(icons::MAGIC_STICK).size(px(14.0)).text_color(
                // The one glance the chip owes the header: an unhealthy rule
                // tints the wand, so trouble is visible without the popover.
                if unhealthy {
                    theme.warning
                } else {
                    theme.text_muted
                },
            ));

        if open {
            let menu = self.render_automations_menu(theme, cx);
            chip = chip.child(popover::anchored_menu("board-automations-menu", menu));
        }
        chip.into_any_element()
    }

    fn render_automations_menu(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mut card = popover::popover_card(theme)
            .w(px(280.0))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.automations_open = false;
                this.automations_dismissed_at = Some(std::time::Instant::now());
                cx.notify();
            }))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(popover::menu_heading(theme, "Automations"));

        match &self.automations {
            None => {
                card = card.child(
                    div()
                        .px(px(10.0))
                        .py(px(6.0))
                        .text_size(px(Theme::TEXT_DENSE))
                        .text_color(theme.text_subtle)
                        .child(SharedString::from("Reading…")),
                );
            }
            Some(view) => {
                let active = view.enabled_count();
                let unhealthy = view.unhealthy_count();
                let mut summary = format!("{active} active");
                if unhealthy > 0 {
                    summary.push_str(&format!(" · {unhealthy} unhealthy"));
                }
                if let Some(next) = view.next_eval_secs {
                    summary.push_str(&format!(
                        " · next check in {}",
                        board::format_age(next as i64)
                    ));
                }
                card = card.child(
                    div()
                        .px(px(10.0))
                        .pb(px(4.0))
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(theme.text_subtle)
                        .child(SharedString::from(summary)),
                );
                // The most recent action or refusal, in the log's own words.
                if let Some(latest) = view.recent.first() {
                    let line = format!(
                        "{} {} — {}",
                        latest.identifier.as_deref().unwrap_or(&latest.rule),
                        latest.decision,
                        latest.reason
                    );
                    card = card.child(
                        div()
                            .px(px(10.0))
                            .pb(px(6.0))
                            .max_w_full()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(theme.text_faint)
                            .child(SharedString::from(line)),
                    );
                }
                if view.rules.is_empty() {
                    card = card.child(
                        div()
                            .px(px(10.0))
                            .py(px(4.0))
                            .text_size(px(Theme::TEXT_DENSE))
                            .text_color(theme.text_subtle)
                            .child(SharedString::from("No rules yet.")),
                    );
                }
                for (ix, status) in view.rules.iter().enumerate() {
                    let name = status.rule.name.clone();
                    let enabled = status.rule.enabled;
                    let verb: SharedString = if enabled { "pause".into() } else { "resume".into() };
                    let state_tone = match status.state.as_str() {
                        "enabled" => theme.settled,
                        "unhealthy" => theme.warning,
                        _ => theme.text_faint,
                    };
                    card = card.child(
                        popover::menu_row(theme, false, format!("board-automation-{ix}"))
                            .id(("board-automation", ix))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.set_automation_enabled(name.clone(), !enabled, cx);
                            }))
                            .child(
                                div()
                                    .size(px(5.0))
                                    .flex_none()
                                    // round-ok: status dot
                                    .rounded_full()
                                    .bg(state_tone),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(SharedString::from(status.rule.name.clone())),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(Theme::TEXT_CAPTION))
                                    .text_color(theme.text_subtle)
                                    .child(verb),
                            ),
                    );
                }
            }
        }

        card = card.child(
            popover::menu_row(theme, false, "board-automations-manage")
                .id("board-automations-manage")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.automations_open = false;
                    cx.emit(BoardEvent::OpenAutomations);
                    cx.notify();
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(SharedString::from("Manage automations…")),
                ),
        );
        card.into_any_element()
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let text: SharedString = if !self.model.rows.is_empty() {
            self.model
                .empty_note()
                .map(Into::into)
                .unwrap_or_else(|| "Nothing on the board.".into())
        } else {
            "Nothing on the board — issues routed by routing.toml appear here.".into()
        };
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .items_center()
            .justify_center()
            .px(px(Theme::SPACE_LG))
            .text_size(px(Theme::TEXT_DENSE))
            .text_color(theme.text_subtle)
            .child(text)
            .into_any_element()
    }

    /// A section header: glyph + label + the count where the rows would be
    /// (comet's board language — see the TUI renderer).
    ///
    /// gh#176 took the slab off it. It was set in caps on a grey fill with its
    /// count in a second grey pill — three devices to say one word, and the
    /// weight landed on the fill rather than on the word. Sentence case at
    /// 12/600 in full-strength text, on a hairline: louder by being quieter,
    /// and the only fills left in the list are the ones that mean something
    /// (hover, selection).
    fn render_section(
        &mut self,
        state: BoardState,
        first: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let selected = self.model.selected.as_deref() == Some(section_row_id(state).as_str());
        let folded = self.model.is_collapsed(state);
        let len = self.model.section_len(state);
        let fade_key = format!("board-section-{}", state.as_str());
        let color = state_color(state, &theme);
        let done = state == BoardState::Done;

        let el = div()
            .id(SharedString::from(format!(
                "board-section-{}",
                state.as_str()
            )))
            .h(px(26.0))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .px(px(PAD_X))
            // The hairlines the header sits between (E4) — the whole separation
            // the canvas gives a group, and the reason it needs no fill of its
            // own. The topmost header skips the top one: the panel's header
            // already drew a line there, and two hairlines a pixel apart is a
            // border.
            .when(!first, |el| el.border_t_1())
            .border_b_1()
            .border_color(theme.border)
            .cursor_pointer()
            // [`Bed::Card`], not `Shell`: this list is inside the main panel —
            // the reference's `--card` — so a selected row here is its
            // `--selcard`. In dark the two land on the same tone; in light the
            // panel is white and the row has to step DOWN into it (gh#258).
            // At rest this paints nothing, which is what E4 asks for; the only
            // fills a header takes are the pointer's and the cursor's, and the
            // canvas has no state for either.
            .list_row(&theme, Bed::Card, selected, &fade_key)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.model.toggle_collapsed(state);
                cx.notify();
            }))
            .child(
                div()
                    .flex_none()
                    .text_size(px(Theme::TEXT_DENSE))
                    // Done's glyph is `--faint` rather than its hue (E12): a
                    // closed row spends no colour, and neither does its header.
                    .text_color(color)
                    .child(SharedString::from(state.glyph())),
            )
            // A header managing a hundred rows carries the weight of one
            // (gh#125) — and it carries it in the text, at full strength,
            // rather than in a fill behind it (gh#176).
            .child(
                div()
                    .flex_none()
                    .text_size(px(Theme::TEXT_DENSE))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    // Done today is quieter than the queue by design (E12).
                    .text_color(if done { theme.text_muted } else { theme.text })
                    .child(SharedString::from(section_title(state))),
            )
            // A bare number (E4). The board says "how many" in one language
            // from the panel's top to its bottom, and a leading `·` is a
            // separator with nothing on the other side of it.
            .child(
                div()
                    .flex_none()
                    .text_size(px(Theme::TEXT_DENSE))
                    .text_color(theme.text_subtle)
                    .child(SharedString::from(len.to_string())),
            )
            .child(div().flex_1())
            // The chevron belongs to the group that is *foldable in the
            // picture* (E12): Done carries one always, because it is history
            // and folding it is the affordance; every other group grows one the
            // moment it is folded, since a header with rows hidden under it and
            // nothing saying so is a section that looks empty.
            .when(done || folded, |el| {
                el.child(
                    icon(if folded {
                        icons::ALT_ARROW_RIGHT
                    } else {
                        icons::ALT_ARROW_DOWN
                    })
                    .size(px(12.0))
                    .text_color(theme.text_subtle),
                )
            });

        el.into_any_element()
    }

    /// A route's group header inside a section (gh#125, E11): the route's name
    /// and its count — "tally 34" is what makes 124 rows scannable.
    ///
    /// A 24px line indented to x=24, the name 12px `--subtle` and the count
    /// 12px `--faint`: quieter than the group header above it in both tone and
    /// height, because it divides a group rather than announcing one. `None` is
    /// the `no route` group, which starts folded at the bottom of its section:
    /// those rows are visibility-only by design, worth a headline and never
    /// pole position.
    fn render_group(
        &mut self,
        state: BoardState,
        route: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let row_id = group_row_id(state, route.as_deref());
        let selected = self.model.selected.as_deref() == Some(row_id.as_str());
        let folded = self.model.is_group_collapsed(state, route.as_deref());
        let len = self.model.group_len(state, route.as_deref());
        let label: SharedString = route
            .as_deref()
            .unwrap_or(board::NO_ROUTE)
            .to_string()
            .into();
        let unrouted = route.is_none();
        let fade_key = format!("board-group-{}-{}", state.as_str(), label);
        let toggle_route = route.clone();

        div()
            .id(SharedString::from(format!(
                "board-group-{}-{}",
                state.as_str(),
                label
            )))
            .h(px(24.0))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .pl(px(24.0))
            .pr(px(PAD_X))
            .cursor_pointer()
            .list_row(&theme, Bed::Card, selected, &fade_key)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.model.toggle_group(state, toggle_route.as_deref());
                cx.notify();
            }))
            .child(
                div()
                    .flex_none()
                    .max_w(px(180.0))
                    .truncate()
                    .text_size(px(Theme::TEXT_DENSE))
                    // The no-route group's headline uses the rows' own words,
                    // in the quiet tone of something you cannot dispatch.
                    .text_color(if unrouted {
                        theme.text_faint
                    } else if selected {
                        theme.text
                    } else {
                        theme.text_subtle
                    })
                    .child(label),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(Theme::TEXT_DENSE))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(len.to_string())),
            )
            // Same rule as the group header above it: the chevron appears when
            // there is something folded away to say so about.
            .when(folded, |el| {
                el.child(div().flex_1()).child(
                    icon(icons::ALT_ARROW_RIGHT)
                        .size(px(12.0))
                        .text_color(theme.text_subtle),
                )
            })
            .into_any_element()
    }

    /// One task row, on one line, in fixed columns (E6): the status glyph, the
    /// id, the title, the repo, the time, then the row's own verb.
    ///
    /// gh#176 put the row's facts in a `·`-joined string at the right edge,
    /// which reads as a sentence whose length depends on the row — so no two
    /// rows agree on where anything is, and the title truncates at a different
    /// place on every line. The canvas spends the same pixels on three columns
    /// that start where they start: an id you can scan down, the repo the work
    /// lands in, and the one clock the row's state measures itself by.
    ///
    /// **Every row is exactly [`ROW_H`] tall, always** (gh#132). gh#125 gave the
    /// row under the pointer a second title line, which meant the list reflowed
    /// on every hover — the "laggy or jagged" the operator reported. Hover here
    /// changes colour and nothing else: the second verb belongs to the SELECTED
    /// row (E10), not the hovered one, so nothing slides sideways as the
    /// pointer crosses a row — least of all a destructive verb arriving under
    /// a pointer that was aimed at the chip beside it.
    ///
    /// The full title lives in the peek panel, which is what a click opens.
    #[allow(clippy::too_many_arguments)]
    fn render_task(&mut self, row: &TaskRow, selected: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let state = row.state();
        let color = state_color(state, &theme);
        let fade_key = format!("board-row-{}", row.id);
        let now = Utc::now();
        let id = row.id.clone();
        let select_id = id.clone();
        let open_id = id.clone();
        // The upstream spelling, not the repo-qualified one (gh#125): the repo
        // has a column of its own now, and saying it twice cost the id column
        // the width that lets `TAL-218` and `gh#144` line up under each other.
        let identifier = row.identifier.clone();
        let repo = repo_cell(row);
        let time = time_cell(row, now);
        let title = row.title.clone();
        let done = state == BoardState::Done;

        let actions = self.render_row_actions(row, selected, cx);

        div()
            .id(SharedString::from(format!("board-row-{}", row.id)))
            // Fixed, not min (gh#132): a row that can grow is a row the list
            // reflows around, and the only thing that ever grew one was the
            // pointer passing over it.
            .h(px(ROW_H))
            .py(px(ROW_PAD_Y))
            .flex_none()
            .flex()
            .flex_col()
            .justify_center()
            .px(px(PAD_X))
            .cursor_pointer()
            .list_row(&theme, Bed::Card, selected, &fade_key)
            // A row is a door (gh#132): clicking one selects it AND opens the
            // peek, because a truncated title that answers a click with nothing
            // is what made the extra text feel like a tooltip. `enter` still
            // releases — reading is never in the way of dispatching.
            .on_click(cx.listener(move |this, _, _, cx| {
                this.model.selected = Some(select_id.clone());
                this.open_peek(cx);
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    if event.click_count == 2 {
                        this.model.selected = Some(open_id.clone());
                        match this.model.task(&open_id).map(|r| r.state()) {
                            Some(BoardState::Ready | BoardState::Failed) => {
                                this.dispatch(&open_id, cx)
                            }
                            Some(BoardState::Working | BoardState::Blocked) => {
                                this.open_chat(&open_id, window, cx)
                            }
                            _ => {}
                        }
                    }
                }),
            )
            // The one line, in the canvas's columns (E6). Its height is the
            // action chip's, so a row that draws a second verb is exactly as
            // tall as one that does not (gh#132).
            .child(
                div()
                    .w_full()
                    .h(px(ROW_LINE_H))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        div()
                            .flex_none()
                            .w(px(COL_GLYPH))
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(color)
                            .child(SharedString::from(state.glyph())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(COL_ID))
                            .truncate()
                            .font_family(theme.font_mono.clone())
                            .text_size(px(Theme::TEXT_CAPTION))
                            // E7/E8/E13: the id trails the title by one tone,
                            // whatever tone the title is in.
                            .text_color(if done {
                                theme.text_faint
                            } else if selected {
                                theme.text_muted
                            } else {
                                theme.text_subtle
                            })
                            .child(SharedString::from(identifier.clone())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            // One line, on every row, in every state (gh#132).
                            // The whole title is in the peek panel.
                            .truncate()
                            .text_size(px(Theme::TEXT_BODY))
                            .text_color(if done {
                                theme.text_subtle
                            } else if selected {
                                theme.text
                            } else {
                                theme.text_muted
                            })
                            .child(SharedString::from(title.clone())),
                    )
                    // The repo and the clock: fixed columns, right-aligned, so
                    // they read DOWN the list. Empty is a width, not a gap — a
                    // ready row with no clock keeps the column open under the
                    // running rows above it.
                    .child(
                        div()
                            .flex_none()
                            .w(px(COL_REPO))
                            .truncate()
                            .text_align(gpui::TextAlign::Right)
                            .text_size(px(Theme::TEXT_DENSE))
                            .text_color(theme.text_subtle)
                            .child(SharedString::from(repo)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(COL_TIME))
                            .truncate()
                            .text_align(gpui::TextAlign::Right)
                            .text_size(px(Theme::TEXT_DENSE))
                            // On a done row this column carries the agent that
                            // did it, in history's own tone (E13).
                            .text_color(if done {
                                theme.text_faint
                            } else {
                                theme.text_subtle
                            })
                            .child(SharedString::from(time)),
                    )
                    // Last (E6). A done row has no verb and draws nothing here.
                    .child(actions),
            )
            .into_any_element()
    }

    /// The row's verbs: the primary one always, the rest on the selected row.
    ///
    /// *Which* actions a row has is [`board::row_actions`] and *which of them is
    /// its own* is [`board::primary_action`] — the shared rules the TUI's keys
    /// and the phone's chip read too (gh#132, gh#176). This decides only how
    /// they look and what a click runs.
    ///
    /// The primary is the chipped one and it comes first (E9): 22px on `--chip`
    /// with `--text` copy — the token every other chip in the window sits on,
    /// rather than the ad-hoc wash this row used to mix for itself. Its copy is
    /// `--text` whatever the verb is: `Open PR` was drawn in the review hue and
    /// that spends a status colour on something that is not a status — the four
    /// hues mean blocked, working, review, settled, and a verb painted in one of
    /// them is a row claiming a state it is not in. The rest follow it bare
    /// (E10) — same box, no bed — where `Cancel` keeps the danger hue, which is
    /// the one case where the colour IS about what the verb does to a run.
    ///
    /// A panel whose every verb appeared only under the pointer read as inert to
    /// anyone who had not already learned the footer's legend; one verb that is
    /// simply there answers "what does this row do" without being asked, and it
    /// is the verb `enter` runs, so the mouse and the keyboard teach the same
    /// lesson.
    fn render_row_actions(
        &mut self,
        row: &TaskRow,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let primary = board::primary_action(row);
        let mut actions: Vec<RowAction> = primary.into_iter().collect();
        if selected {
            actions.extend(board::secondary_actions(row));
        }
        if actions.is_empty() {
            return gpui::Empty.into_any_element();
        }
        let theme = Theme::of(cx).clone();
        let id = row.id.clone();
        div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(5.0))
            .children(actions.into_iter().map(|action| {
                let target = id.clone();
                let chipped = Some(action) == primary;
                div()
                    .id(SharedString::from(format!(
                        "board-action-{}-{}",
                        action_key(action),
                        id
                    )))
                    .flex_none()
                    .h(px(ROW_LINE_H))
                    .px(px(9.0))
                    .rounded(px(Theme::RADIUS_CHIP))
                    .when(chipped, |el| el.bg(theme.chip))
                    .flex()
                    .items_center()
                    .text_size(px(Theme::TEXT_DENSE))
                    .text_color(if chipped {
                        theme.text
                    } else {
                        action_color(action, &theme)
                    })
                    .hover(|s| s.bg(theme.wash(0.12)))
                    .child(SharedString::from(action.short_label()))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.run_action(&target, action, window, cx);
                    }))
            }))
            .into_any_element()
    }

    /// Do one of the shared actions to a row.
    ///
    /// The single place this surface *performs* a board action, so the row's
    /// chips and the peek panel's buttons cannot drift into meaning different
    /// things by the same word. A release goes through the dispatch picker
    /// exactly as `enter` does — reading a row never skips the account question.
    fn run_action(
        &mut self,
        id: &str,
        action: RowAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            RowAction::Dispatch | RowAction::Retry => self.dispatch(id, cx),
            RowAction::Cancel => self.cancel(id, cx),
            RowAction::OpenChat => self.open_chat(id, window, cx),
            RowAction::OpenIssue | RowAction::OpenPr => {
                let url = self
                    .model
                    .task(id)
                    .and_then(|row| board::action_url(row, action))
                    .map(str::to_string);
                match url {
                    Some(url) => self.open_pr_url(&url, cx),
                    None => self.set_notice("Nothing to open there yet", cx),
                }
            }
        }
    }

    fn open_pr_url(&mut self, url: &str, cx: &mut Context<Self>) {
        cx.open_url(url);
    }

    /// The dispatch picker: the runtime strip, the account strip, then a
    /// type-to-filter model search and the filtered, scrollable list of that
    /// harness's models. Clicking a runtime SELECTS it and loads its models;
    /// clicking an account SELECTS whose subscription the run spends; typing
    /// narrows the model list (matched against id OR label); clicking a model
    /// dispatches immediately; enter dispatches with all three highlights.
    /// Before the options load each strip is a single "Loading…" label.
    fn render_dispatch_picker(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let draft = self
            .dispatch
            .clone()
            .expect("picker renders only when open");
        let runtime_focused = draft.row == PickerRow::Runtime;
        let model_focused = draft.row == PickerRow::Model;
        let query = self.dispatch_search.read(cx).text().to_string();

        // Row 1: the runtime. Highlight = the route's runtime; the label shows
        // which row the keyboard is on. Runtimes are few — chips stay.
        //
        // When the highlighted runtime cannot start on the host, the label says
        // so instead of saying "Runtime" (gh#187). The host refuses that
        // dispatch before it cuts anything, so this is the one place the
        // operator can learn why without pressing enter first — and it is the
        // *highlighted* one, because that is the one enter would send.
        let blocked = draft
            .runtimes
            .get(draft.active_runtime)
            .and_then(|r| Some(r.unavailable?.refusal(&r.name)));
        let runtime_label: SharedString = match (&draft.runtime_error, draft.runtimes.is_empty()) {
            (Some(err), _) => format!("{err} — enter dispatches with the route's runtime").into(),
            (None, true) => "Loading runtimes…".into(),
            // The host's own refusal, verbatim: pressing enter here would print
            // this sentence back, so the picker may as well say it first.
            (None, false) => match &blocked {
                Some(refusal) => SharedString::from(refusal.clone()),
                None => "Runtime".into(),
            },
        };
        let label_color = if blocked.is_some() {
            theme.danger
        } else if runtime_focused {
            theme.accent
        } else {
            theme.text_subtle
        };
        let mut runtime_row = div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(Theme::SPACE_LG))
            .py(px(6.0))
            .child(
                div()
                    .flex_none()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(label_color)
                    .child(runtime_label),
            );

        for (ix, runtime) in draft.runtimes.iter().enumerate() {
            let active = ix == draft.active_runtime;
            let name = runtime.name.clone();
            // An unavailable runtime is still offered, dimmed and marked
            // (gh#187): filtering it away would leave an operator who expects
            // OpenCode on the box wondering whether they misremembered which
            // box it was, and it is still selectable so the label above can
            // tell them what to do about it.
            let unavailable = runtime.unavailable;
            let label = match unavailable {
                Some(why) => format!("{} · {}", runtime.label, why.reason()),
                None => runtime.label.clone(),
            };
            let key = format!("board-runtime-{}-{}", draft.task_id, name);
            runtime_row = runtime_row.child(
                div()
                    .id(key)
                    .flex_none()
                    .h(px(22.0))
                    .px(px(9.0))
                    .rounded(px(Theme::RADIUS_CHIP))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .bg(if active && runtime_focused {
                        theme.accent.opacity(0.16)
                    } else if active {
                        theme.accent.opacity(0.08)
                    } else {
                        theme.wash(0.05)
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.select_runtime(&name, window, cx);
                    }))
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(if unavailable.is_some() {
                                // Dimmed whether or not it is the highlight:
                                // this one is a fact about the host, not about
                                // where the cursor happens to be.
                                theme.text_faint
                            } else if active {
                                if runtime_focused {
                                    theme.accent
                                } else {
                                    // gh#172: a selected row is `text`, not a
                                    // ninth of a tone off muted.
                                    theme.text
                                }
                            } else {
                                theme.text_muted
                            })
                            .child(SharedString::from(label)),
                    ),
            );
        }

        // Row 2: whose subscription the run spends (gh#74).
        let account_row = self.render_account_strip(&draft, &theme, cx);

        // Row 3: the model search + a "shown of total" count.
        let (total_models, filtered): (usize, Vec<usize>) = match draft.catalog() {
            Some(ModelCatalog::Ready(models)) => {
                (models.len(), filtered_model_indices(models, &query))
            }
            _ => (0, Vec::new()),
        };
        let models_ready = matches!(draft.catalog(), Some(ModelCatalog::Ready(_)));
        let mut model_row = div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .px(px(Theme::SPACE_LG))
            .py(px(6.0))
            .border_t_1()
            .border_color(theme.white_alpha(0.06))
            .child(
                div()
                    .flex_none()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(if model_focused {
                        theme.accent
                    } else {
                        theme.text_subtle
                    })
                    .child(SharedString::from("Model")),
            )
            .child(
                // The type-to-filter input (PaletteSearch context: typing
                // narrows the list, ↑↓/←→/enter bubble to the board frame).
                div()
                    .flex_1()
                    .min_w_0()
                    .h(px(24.0))
                    .px(px(8.0))
                    .rounded(px(Theme::RADIUS_CHIP))
                    .border_1()
                    .border_color(if model_focused {
                        theme.border_strong
                    } else {
                        theme.border
                    })
                    .bg(theme.wash(0.05))
                    .flex()
                    .items_center()
                    .child(self.dispatch_search.clone()),
            );
        if models_ready {
            model_row = model_row.child(
                div()
                    .flex_none()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(theme.text_subtle)
                    .child(SharedString::from(format!(
                        "{}/{}",
                        filtered.len(),
                        total_models
                    ))),
            );
        }

        // Row 3: the filtered model list — vertical and scrollable now that a
        // search narrows it (the old horizontal chip strip was unusable at
        // 70+ models).
        let list: AnyElement = match draft.catalog() {
            Some(ModelCatalog::Error(err)) => div()
                .flex_none()
                .px(px(Theme::SPACE_LG))
                .py(px(6.0))
                .text_size(px(Theme::TEXT_CAPTION))
                .text_color(theme.warning)
                .child(SharedString::from(format!(
                    "{err} — enter dispatches with the harness default"
                )))
                .into_any_element(),
            Some(ModelCatalog::Ready(_)) if filtered.is_empty() => div()
                .flex_none()
                .px(px(Theme::SPACE_LG))
                .py(px(8.0))
                .text_size(px(Theme::TEXT_CAPTION))
                .text_color(theme.text_subtle)
                .child(SharedString::from(format!("No models match “{query}”")))
                .into_any_element(),
            Some(ModelCatalog::Ready(models)) => {
                let rows: Vec<AnyElement> = filtered
                    .into_iter()
                    .enumerate()
                    .map(|(display_ix, catalog_ix)| {
                        self.render_model_row(
                            &draft,
                            display_ix,
                            catalog_ix,
                            &models[catalog_ix],
                            &theme,
                            cx,
                        )
                    })
                    .collect();
                div()
                    .id("board-model-list")
                    .flex_none()
                    .max_h(px(184.0))
                    .overflow_y_scroll()
                    .track_scroll(&self.model_scroll)
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .px(px(Theme::SPACE_LG))
                    .py(px(4.0))
                    .children(rows)
                    .into_any_element()
            }
            _ => div()
                .flex_none()
                .px(px(Theme::SPACE_LG))
                .py(px(6.0))
                .text_size(px(Theme::TEXT_CAPTION))
                .text_color(theme.text_subtle)
                .child(SharedString::from("Loading models…"))
                .into_any_element(),
        };

        // Row 5: the key hint.
        let hint = div()
            .flex_none()
            .px(px(Theme::SPACE_LG))
            .py(px(6.0))
            .border_t_1()
            .border_color(theme.white_alpha(0.06))
            .child(
                div()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(theme.text_subtle)
                    .child(SharedString::from(
                        "type to filter · ↑↓ switch · ←→ pick · enter dispatch · esc cancel",
                    )),
            );

        div()
            .id("board-dispatch-picker")
            .flex_none()
            .flex()
            .flex_col()
            .child(runtime_row)
            .child(account_row)
            .child(model_row)
            .child(list)
            .child(hint)
            .into_any_element()
    }

    /// The account strip: which saved login this dispatch spends (gh#74).
    ///
    /// Chips, like the runtime row — a box has a handful of logins, not a
    /// catalog. The first chip is the route's own account: dispatching on it
    /// sends no override, which is exactly what every dispatch did before this
    /// row existed. The rest are the logins the highlighted runtime's harness
    /// can actually spend; a harness with none saved leaves just the default,
    /// which is the honest answer for a single-account box.
    fn render_account_strip(
        &self,
        draft: &DispatchDraft,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let focused = draft.row == PickerRow::Account;
        let options = draft.account_options();
        let label: SharedString = match (&draft.accounts, options.is_empty()) {
            (AccountCatalog::Error(err), _) => {
                format!("{err} — enter dispatches on the route's").into()
            }
            (AccountCatalog::Loading, _) => "Loading accounts…".into(),
            (AccountCatalog::Ready(_), true) => "Account · none saved here".into(),
            (AccountCatalog::Ready(_), false) => "Account".into(),
        };
        // Row 0 names what "no override" spends, so the default is a choice
        // rather than a blank: the route's account when it has one, otherwise
        // the device's own CLI login. When either one is somebody else's, it
        // says whose instead — this is the chip an enter-enter release lands
        // on, and a slot id is not an answer to "who pays" (gh#101).
        // Row 0's *effective* slot is the route's account, not "no account" —
        // sending no override is what makes the route's the one that pays.
        let default_bills = draft.bills(draft.route_account.as_deref());
        let default_label = match (&default_bills, draft.route_account.as_deref()) {
            (Some(bills), _) => format!("Route default · {bills}"),
            (None, Some(account)) => format!("Route default · {account}"),
            (None, None) => "Route default".to_string(),
        };
        let chips: Vec<(usize, String, bool)> =
            std::iter::once((0, default_label, default_bills.is_some()))
                .chain(options.iter().enumerate().map(|(ix, account)| {
                    match draft.bills(Some(&account.id)) {
                        Some(bills) => (
                            ix + 1,
                            format!("{} · {bills}", account_label(account)),
                            true,
                        ),
                        None => (ix + 1, account_label(account), false),
                    }
                }))
                .collect();

        let mut row = div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(Theme::SPACE_LG))
            .py(px(6.0))
            .border_t_1()
            .border_color(theme.white_alpha(0.06))
            .child(
                div()
                    .flex_none()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(if focused {
                        theme.accent
                    } else {
                        theme.text_subtle
                    })
                    .child(label),
            );
        for (ix, chip_label, bills_somebody_else) in chips {
            let active = ix == draft.active_account;
            let key = format!("board-account-{}-{ix}", draft.task_id);
            // A chip that charges somebody else keeps the amber whether or not
            // it is highlighted: the selection accent says "you are here", and
            // overwriting the warning with it would hide the thing exactly when
            // the operator is about to press enter on it.
            let warned = theme.warning_text();
            row = row.child(
                div()
                    .id(key)
                    .flex_none()
                    .h(px(22.0))
                    .px(px(9.0))
                    .rounded(px(Theme::RADIUS_CHIP))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .when(bills_somebody_else, |chip| {
                        chip.border_1().border_color(theme.warning.opacity(0.5))
                    })
                    .bg(if bills_somebody_else && active {
                        theme.warning.opacity(0.16)
                    } else if bills_somebody_else {
                        theme.warning.opacity(0.08)
                    } else if active && focused {
                        theme.accent.opacity(0.16)
                    } else if active {
                        theme.accent.opacity(0.08)
                    } else {
                        theme.wash(0.05)
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.select_account(ix, cx);
                    }))
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(if bills_somebody_else {
                                warned
                            } else if active && focused {
                                theme.accent
                            } else {
                                theme.text_muted
                            })
                            .child(SharedString::from(chip_label)),
                    ),
            );
        }
        row.into_any_element()
    }

    /// One model row in the picker's filtered list. The catalog's first row —
    /// the harness default — sends no override (the route's behavior); every
    /// other row is that catalog model as an explicit override. Rows carry
    /// their CATALOG index, so a click dispatches the right model even when a
    /// filter reorders what is visible.
    fn render_model_row(
        &mut self,
        draft: &DispatchDraft,
        display_ix: usize,
        catalog_ix: usize,
        model: &BoardModelInfo,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = display_ix == draft.active_model;
        let focused = draft.row == PickerRow::Model;
        let label: SharedString = model.label.clone().into();
        let id: SharedString = model.id.clone().into();
        let is_default = catalog_ix == 0;
        let key = format!("board-model-{}-{}", draft.task_id, catalog_ix);
        div()
            .id(key)
            .flex_none()
            .px(px(9.0))
            .py(px(4.0))
            .rounded(px(Theme::RADIUS_CHIP))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .cursor_pointer()
            .bg(if active && focused {
                theme.accent.opacity(0.16)
            } else if active {
                theme.accent.opacity(0.08)
            } else {
                theme.wash(0.0)
            })
            .hover(|s| s.bg(theme.wash(0.1)))
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.confirm_with_model(catalog_ix, cx);
                // The picker closes on the click — hand keyboard control back
                // to the board (the model filter may have held focus).
                if this.dispatch.is_none() {
                    window.focus(&this.focus_handle, cx);
                }
            }))
            .child(
                // Name + muted id subline — the id is what the filter matches,
                // so surfacing it makes the narrowing legible.
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .w_full()
                            .truncate()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(if active && focused {
                                theme.accent
                            } else if active {
                                theme.text_muted
                            } else {
                                theme.text
                            })
                            .child(label),
                    )
                    .child(
                        div()
                            .w_full()
                            .truncate()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(theme.text_subtle)
                            .child(id),
                    ),
            )
            .when(is_default, |el| {
                el.child(
                    div()
                        .flex_none()
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(theme.text_subtle)
                        .child(SharedString::from("default")),
                )
            })
            .into_any_element()
    }

    /// The stack map: every layer of this row's stack, bottom first, each one a
    /// door to the sibling that carries it (gh#283).
    ///
    /// GitHub's stack map, board-side. Without it a five-layer stack is five
    /// unrelated rows in five different sections of the board, and reading the
    /// one in front of you tells you nothing about the four whose merge state
    /// decides whether this one can land.
    ///
    /// The chips carry what the row already knows about each layer, so nothing
    /// here waits on a fetch: the layer you are on is accented, one GitHub
    /// objects to is drawn in the failed colour, and one that has already
    /// landed is muted — it is history in the chain rather than an obstacle.
    fn render_stack_map(
        &self,
        row: &TaskRow,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let layers = board::stack_map(row);
        if layers.is_empty() {
            return None;
        }
        let here = row.id.clone();
        Some(
            div()
                .flex_none()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap(px(4.0))
                .children(layers.iter().enumerate().map(|(ix, layer)| {
                    let current = layer.id == here;
                    let stuck = layer
                        .mergeable
                        .as_deref()
                        .is_some_and(|state| state != "clean");
                    let colour = if current {
                        theme.accent
                    } else if !layer.open {
                        theme.text_faint
                    } else if stuck {
                        state_color(BoardState::Failed, theme)
                    } else {
                        theme.text_muted
                    };
                    let label = board::layer_label(layer);
                    let target = layer.id.clone();
                    div()
                        .id(SharedString::from(format!("board-peek-layer-{}", layer.id)))
                        .flex_none()
                        .px(px(6.0))
                        .h(px(18.0))
                        .flex()
                        .items_center()
                        .rounded(px(Theme::RADIUS_CHIP))
                        .bg(theme.wash(if current { 0.16 } else { 0.08 }))
                        .font_family(theme.font_mono.clone())
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(colour)
                        .hover(|s| s.bg(theme.wash(0.18)))
                        // Bottom first, left to right, and the arrow says which
                        // way the chain merges rather than leaving a row of
                        // chips to be read as a set.
                        .child(SharedString::from(if ix == 0 {
                            label
                        } else {
                            format!("↑ {label}")
                        }))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            // A layer GitHub named but the board does not hold
                            // — a stack reaching into a repo nobody polls — is
                            // not on the board to open. Leave the peek where it
                            // is rather than blanking it.
                            if this.model.task(&target).is_some() {
                                this.model.selected = Some(target.clone());
                                this.open_peek(cx);
                                cx.notify();
                            }
                        }))
                }))
                .into_any_element(),
        )
    }

    /// The peek panel: the selected row, in full (gh#132).
    ///
    /// What the list cannot say, said once and where you asked for it — the
    /// whole title, the issue body as markdown, the labels, where the work sits,
    /// what has been tried on it, and every action the row has anywhere else.
    /// It is for *reading*: `enter` still releases from the list, and nothing
    /// here is a step on the way to a dispatch.
    fn render_peek(&mut self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let Some(row) = self.model.selected_task().cloned() else {
            // The cursor is on a header. Draw nothing rather than a card about
            // no row — the peek reopens itself the moment it lands on one.
            return gpui::Empty.into_any_element();
        };
        let now = Utc::now();
        let state = row.state();
        let id = row.id.clone();
        let loading = self.detail_pending.as_deref() == Some(id.as_str());
        let body = self
            .detail
            .as_ref()
            .filter(|d| d.id == id)
            .and_then(|d| d.body.clone());
        let error = self.detail_error.clone();

        // Line 1 of the card: which row this is, and the way out.
        let heading = div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(7.0))
            .child(
                div()
                    .flex_none()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(state_color(state, &theme))
                    .child(SharedString::from(state.glyph())),
            )
            .child(
                div()
                    .flex_none()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(row.display_identifier())),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("board-peek-close")
                    .flex_none()
                    .px(px(6.0))
                    .h(px(18.0))
                    .flex()
                    .items_center()
                    .rounded(px(Theme::RADIUS_CHIP))
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(theme.text_subtle)
                    .hover(|s| s.bg(theme.wash(0.12)))
                    .child(SharedString::from("esc"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.peek = false;
                        cx.notify();
                    })),
            );

        // The whole title — the sentence gh#125 tried to fit into a second row
        // of a list that then reflowed under the pointer.
        let title = div()
            .flex_none()
            .text_size(px(Theme::TEXT_BODY))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.text)
            .child(SharedString::from(row.title.clone()));

        let facts: Vec<AnyElement> = [
            board::placement_line(&row),
            // Where this layer sits and where the chain lands (gh#283). Above
            // the history, because it is a fact about the work in front of you
            // rather than about what has been tried on it.
            board::stack_line(&row),
            board::history_line(&row, now),
            // Automation provenance (gh#490): which rule released this and
            // whose automation that is. Nothing on rows a person dispatched.
            board::automation_line(&row),
        ]
        .into_iter()
        .flatten()
        .map(|line| {
            div()
                .flex_none()
                .text_size(px(Theme::TEXT_CAPTION))
                .text_color(theme.text_subtle)
                .child(SharedString::from(line))
                .into_any_element()
        })
        .collect();

        let stack_map = self.render_stack_map(&row, &theme, cx);

        // The issue's own labels — the one thing on the row the list has never
        // had room for at all.
        let labels: Option<AnyElement> = (!row.labels.is_empty()).then(|| {
            div()
                .flex_none()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(4.0))
                .children(row.labels.iter().map(|label| {
                    div()
                        .flex_none()
                        .px(px(6.0))
                        .h(px(16.0))
                        .flex()
                        .items_center()
                        .rounded(px(Theme::RADIUS_CHIP))
                        .bg(theme.wash(0.08))
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(label.clone()))
                }))
                .into_any_element()
        });

        // The body. Rendered with the transcript's markdown pipeline, so an
        // issue reads here the way an agent's reply reads there.
        let body_element: AnyElement = if let Some(message) = error {
            div()
                .text_size(px(Theme::TEXT_CAPTION))
                .text_color(theme.warning)
                .child(message)
                .into_any_element()
        } else if loading {
            div()
                .text_size(px(Theme::TEXT_CAPTION))
                .text_color(theme.text_subtle)
                .child(SharedString::from("Reading the issue…"))
                .into_any_element()
        } else if let Some(text) = body {
            let tree = crate::markdown::parser::parse_full(&text);
            crate::markdown::render::render_tree(
                &tree,
                &crate::markdown::render::RenderOptions::settled_copyable(SharedString::from(
                    format!("board-peek-{id}"),
                )),
                &theme,
                window,
                &|_| None,
            )
        } else {
            div()
                .text_size(px(Theme::TEXT_CAPTION))
                .text_color(theme.text_faint)
                .child(SharedString::from(board::NO_BODY))
                .into_any_element()
        };

        let actions = board::detail_actions(&row);
        // The review (gh#180) leads the chip row on any row that has been
        // attempted, and it is spelled for what it opens rather than for what
        // it is: "what changed" is the question, and it is not the PR's answer
        // to it. Deliberately NOT a `RowAction`: the shared action set is the
        // verbs the *board* offers on a row, and this one navigates the desktop
        // window — a phone drawing a chip for a screen it does not have would
        // be a promise the set cannot keep.
        let reviewable = board::reviewable(&row);
        let action_row: Option<AnyElement> = (reviewable || !actions.is_empty()).then(|| {
            let review_id = id.clone();
            div()
                .flex_none()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(5.0))
                .when(reviewable, |el| {
                    el.child(
                        div()
                            .id(SharedString::from(format!("board-peek-review-{id}")))
                            .flex_none()
                            .h(px(22.0))
                            .px(px(9.0))
                            .rounded(px(Theme::RADIUS_CHIP))
                            .bg(theme.wash(0.12))
                            .flex()
                            .items_center()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(theme.accent)
                            .hover(|s| s.bg(theme.wash(0.18)))
                            .child(SharedString::from("Review changes"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.open_review(&review_id, cx);
                            })),
                    )
                })
                .children(actions.into_iter().map(|action| {
                    let target = id.clone();
                    div()
                        .id(SharedString::from(format!(
                            "board-peek-{}-{}",
                            action_key(action),
                            id
                        )))
                        .flex_none()
                        .h(px(22.0))
                        .px(px(9.0))
                        .rounded(px(Theme::RADIUS_CHIP))
                        .bg(theme.wash(0.12))
                        .flex()
                        .items_center()
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(action_color(action, &theme))
                        .hover(|s| s.bg(theme.wash(0.18)))
                        // The full spelling here: a panel has room, and a
                        // button labelled "Open" beside another labelled
                        // "Open PR" is a coin toss.
                        .child(SharedString::from(action.label()))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.run_action(&target, action, window, cx);
                        }))
                }))
                .into_any_element()
        });

        let card = div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .px(px(Theme::SPACE_LG))
            .py(px(8.0))
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.wash(0.03))
            .child(heading)
            .child(title)
            .children(facts)
            .children(stack_map)
            .children(labels)
            // The body is the only part that can outgrow the panel, so it is
            // the only part that scrolls — bounded here rather than on the card
            // (the model list next door is bounded the same way), so a short
            // issue takes the room it needs and a long one stops at the cap
            // instead of pushing the cursor's own row off screen.
            .child(
                div()
                    .id("board-peek-body")
                    .flex_none()
                    .max_h(px(PEEK_BODY_MAX_H))
                    .overflow_y_scroll()
                    .track_scroll(&self.detail_scroll)
                    .flex()
                    .flex_col()
                    .child(body_element),
            )
            .children(action_row);

        // Opening is a deliberate act and gets an entrance; nothing about the
        // list beneath it moves, so there is no reflow to animate away.
        motion::fade_quick(SharedString::from(format!("board-peek-in-{id}")), card)
            .into_any_element()
    }

    /// The footer: a transient dispatch/cancel message owns it until it
    /// expires, then the board's key hints take over.
    ///
    /// Three hints (E14), not eight: `↵ dispatch · space peek · / find`. The
    /// footer had grown into a legend for every key the panel binds — which is
    /// a list nobody reads, and a list that changes under you as the cursor
    /// moves is worse than one that does not. The keys it stopped naming still
    /// work (`f`/`F` cycle and clear the filter, `r` opens a review, `esc`
    /// closes the panel); the route chip in the header is where filtering
    /// became visible instead.
    ///
    /// `↵`'s verb still follows the selected row, from the same rule the row's
    /// own chip draws (gh#176) — one designation, said twice in the same words.
    fn render_footer(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let has_notice = self.notice.is_some();
        let notice = self.notice.clone();
        let typing = self.model.typing;
        let selected_task = self.model.selected_task().map(|r| r.state());
        let enter_hint = self
            .model
            .selected_task()
            .and_then(board::primary_action)
            .map(|action| format!("↵ {}", action.verb()));
        let picking = self.dispatch.is_some();
        let peek_open = self.peek;

        let content: SharedString = if let Some(notice) = notice {
            notice
        } else if picking {
            "dispatch picker — enter to dispatch · esc cancel".into()
        } else if typing {
            "enter to keep the filter · esc to clear".into()
        } else {
            let mut hints: Vec<&str> = Vec::new();
            if let Some(hint) = enter_hint.as_deref() {
                hints.push(hint);
            }
            // The door, named (gh#132) — an affordance nobody can find is one
            // that does not exist.
            if selected_task.is_some() {
                hints.push(if peek_open {
                    "space close"
                } else {
                    "space peek"
                });
            }
            hints.push("/ find");
            hints.join(" · ").into()
        };

        div()
            .h(px(28.0))
            .flex_none()
            .flex()
            .items_center()
            .px(px(PAD_X))
            .border_t_1()
            .border_color(theme.border)
            .text_size(px(Theme::TEXT_DENSE))
            .text_color(if has_notice {
                theme.warning
            } else {
                theme.text_subtle
            })
            .child(content)
            .into_any_element()
    }
}

actions!(board, [ToggleBoard]);

/// Bind the board keymap (global): Cmd+Shift+B on macOS, Ctrl+Shift+B
/// elsewhere.
pub fn init(cx: &mut App) {
    let toggle = if cfg!(target_os = "macos") {
        "cmd-shift-b"
    } else {
        "ctrl-shift-b"
    };
    cx.bind_keys([gpui::KeyBinding::new(toggle, ToggleBoard, None)]);
}

impl gpui::EventEmitter<BoardEvent> for BoardPanel {}

impl Render for BoardPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let error = self.error.clone();
        let lines = self.model.lines();

        // Keep the find field focused once opened; `set_text` resets it each
        // open so the query starts empty.
        if let (Some(find), true) = (self.find.clone(), self.find_focus_pending) {
            self.find_focus_pending = false;
            let handle = find.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }

        // The body: an empty sentence, or the scrollable lines.
        let body: AnyElement = if lines.is_empty() {
            self.render_empty(cx)
        } else {
            let body_lines: Vec<AnyElement> = lines
                .iter()
                .enumerate()
                .map(|(ix, line)| {
                    let content = match line {
                        BoardLine::Section(state) => self.render_section(*state, ix == 0, cx),
                        BoardLine::Group(state, route) => {
                            self.render_group(*state, route.clone(), cx)
                        }
                        BoardLine::Task(id) => {
                            let selected = self.model.selected.as_deref() == Some(id.as_str());
                            match self.model.task(id).cloned() {
                                Some(row) => self.render_task(&row, selected, cx),
                                None => gpui::Empty.into_any_element(),
                            }
                        }
                    };
                    // Each line is its own stateful child so the scroll handle
                    // can reveal it by index (`scroll_to_item`).
                    div()
                        .id(("board-line", ix))
                        .flex_none()
                        .child(content)
                        .into_any_element()
                })
                .collect();
            div()
                .id("board-body")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .flex()
                .flex_col()
                .children(body_lines)
                .into_any_element()
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            // FIRST child ⇒ paints first: the peek renders through the
            // transcript's markdown pipeline, whose text elements register
            // themselves for selection on every paint. Without this reset the
            // registry grows by an element per painted line per frame for as
            // long as the board is open, and a drag resolves against a stale
            // document (gh#534).
            .child(crate::markdown::render::selection_frame_reset())
            .key_context("Board")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window: &mut Window, cx| {
                    // Clicks on the board body re-arm keyboard navigation; a
                    // click inside the open find field must NOT steal focus
                    // from the input (typing owns it). Same for the dispatch
                    // picker's model filter — it must keep the focus a click
                    // just gave it.
                    if !this.model.typing && this.dispatch.is_none() {
                        window.focus(&this.focus_handle, cx);
                    }
                }),
            )
            .child(self.render_header(cx))
            .when_some(error, |el, message| {
                el.child(
                    div()
                        .flex_none()
                        .px(px(Theme::SPACE_MD))
                        .py(px(4.0))
                        .border_b_1()
                        .border_color(theme.border)
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(theme.warning)
                        .child(message),
                )
            })
            .when(self.dispatch.is_some(), |el| {
                el.child(self.render_dispatch_picker(cx))
            })
            .child(body)
            // The peek sits between the list and the footer: the list keeps the
            // top of the panel (it is what you are navigating), and the row you
            // opened reads directly under it.
            .when(self.peek, |el| el.child(self.render_peek(window, cx)))
            .child(self.render_footer(cx))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use comet_proto::view::board::TaskRow;

    fn row(id: &str, state: BoardState) -> TaskRow {
        TaskRow {
            automation: None,
            automation_owner: None,
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
            // Stamped now, not on a fixed date: `done` is bounded to today by
            // the shared derivation, so a frozen timestamp makes the done
            // section empty on every day but one.
            updated_at: Utc::now().to_rfc3339(),
            started_at: None,
            account: None,
            dispatched_by_user: None,
            dispatched_by_verified: false,
            billed_to: None,
            cross_billed: None,
            max_duration_secs: None,
            context: None,
            stop_reason: None,
        }
    }

    fn model(rows: Vec<TaskRow>) -> BoardModel {
        let mut m = BoardModel::new();
        m.set_rows(rows);
        m
    }

    /// A live attempt must not change colour on its way from the board pane to
    /// the sidebar's Agents section (gh#103) — same state, same accent.
    #[test]
    fn a_live_agent_carries_the_board_pane_colour() {
        use board::AgentState;
        let theme = Theme::dark();
        assert_eq!(
            agent_state_color(AgentState::Blocked, &theme),
            state_color(BoardState::Blocked, &theme)
        );
        // A dead run reads as the board's `failed`, which shares blocked's red;
        // the glyph is what tells them apart, there and here.
        assert_eq!(
            agent_state_color(AgentState::Errored, &theme),
            state_color(BoardState::Failed, &theme)
        );
        assert_eq!(
            agent_state_color(AgentState::Working, &theme),
            state_color(BoardState::Working, &theme)
        );
        assert_ne!(
            agent_state_color(AgentState::Working, &theme),
            agent_state_color(AgentState::Blocked, &theme)
        );
    }

    /// The board pane and the sidebar must agree on every state they BOTH
    /// render (gh#173). They did not: a working agent was amber here and pink
    /// there, one keystroke apart on screen.
    ///
    /// The pairs are the states that name the same thing in both vocabularies.
    /// `AwaitingInput` is deliberately absent: the board has no such state — it
    /// files a question under `blocked` together with the dead runs, and the
    /// Needs-you inbox (gh#122) is where the two are told apart, in the same
    /// review hue the chat dot uses.
    #[test]
    fn the_board_and_the_sidebar_agree_on_every_shared_state() {
        use crate::shell::spaces::status_dot_color;
        use comet_proto::ChatIndicator;
        for theme in [Theme::dark(), Theme::light()] {
            for (state, indicator) in [
                (BoardState::Working, ChatIndicator::Working),
                (BoardState::Failed, ChatIndicator::Errored),
                (BoardState::Blocked, ChatIndicator::Errored),
            ] {
                assert_eq!(
                    state_color(state, &theme),
                    status_dot_color(indicator, &theme),
                    "{state:?} and {indicator:?} disagree"
                );
            }
            // And the sidebar's own two renderings of one live agent agree.
            for (agent, indicator) in [
                (board::AgentState::Working, ChatIndicator::Working),
                (board::AgentState::Errored, ChatIndicator::Errored),
            ] {
                assert_eq!(
                    agent_state_color(agent, &theme),
                    status_dot_color(indicator, &theme),
                    "{agent:?} and {indicator:?} disagree"
                );
            }
            // Working is the ramp's amber now, in both — not a fifth hue.
            assert_eq!(state_color(BoardState::Working, &theme), theme.warning);
            // Ready and done spend no colour at all. (Measured as channel
            // spread, not HSL saturation — light's neutrals carry a trace of
            // blue, and saturation is meaningless that close to white.)
            for state in [BoardState::Ready, BoardState::Done] {
                let tone = state_color(state, &theme);
                assert!(!crate::theme::spends_colour(tone), "{state:?} paints a hue");
            }
            let idle = status_dot_color(ChatIndicator::Idle, &theme);
            assert!(!crate::theme::spends_colour(idle));
        }
    }

    #[test]
    fn lines_group_by_state_in_fixed_order_with_done_folded() {
        let m = model(vec![
            row("1", BoardState::Ready),
            row("2", BoardState::Working),
            row("3", BoardState::Done),
            row("4", BoardState::Blocked),
        ]);
        let lines = m.lines();
        let ids: Vec<String> = lines.iter().map(|l| l.id()).collect();
        // Blocked, working, ready, then the done header (folded — no rows).
        assert_eq!(
            ids,
            vec![
                section_row_id(BoardState::Blocked),
                "4".to_string(),
                section_row_id(BoardState::Working),
                "2".to_string(),
                section_row_id(BoardState::Ready),
                "1".to_string(),
                section_row_id(BoardState::Done),
            ]
        );
    }

    #[test]
    fn done_starts_folded_and_toggles() {
        let mut m = model(vec![row("3", BoardState::Done)]);
        assert!(m.is_collapsed(BoardState::Done));
        assert_eq!(m.lines().len(), 1, "only the folded header draws");
        m.toggle_collapsed(BoardState::Done);
        assert!(!m.is_collapsed(BoardState::Done));
        assert_eq!(m.lines().len(), 2, "rows return on expand");
        assert_eq!(m.section_len(BoardState::Done), 1);
    }

    #[test]
    fn a_section_with_several_routes_grows_group_headers_no_route_folded_last() {
        let mut rows = vec![
            row("1", BoardState::Ready),
            row("2", BoardState::Ready),
            row("3", BoardState::Ready),
        ];
        rows[1].route = Some("tally".into());
        rows[2].route = None;
        rows[2].dispatchable = false;
        let mut m = model(rows);
        let ids: Vec<String> = m.lines().iter().map(|l| l.id()).collect();
        assert_eq!(
            ids,
            vec![
                section_row_id(BoardState::Ready),
                group_row_id(BoardState::Ready, Some("offhand")),
                "1".to_string(),
                group_row_id(BoardState::Ready, Some("tally")),
                "2".to_string(),
                // The no-route group draws folded at the bottom: a headline,
                // not pole position — its row is hidden.
                group_row_id(BoardState::Ready, None),
            ]
        );
        // The first task the cursor lands on is dispatchable, not `no route`.
        m.clamp_selection();
        assert_eq!(m.selected.as_deref(), Some("1"));
        // Enter on the group header brings the rows back.
        m.toggle_group(BoardState::Ready, None);
        assert!(m.lines().iter().any(|l| l.id() == "3"));
        assert_eq!(m.group_len(BoardState::Ready, None), 1);
    }

    #[test]
    fn a_single_routed_group_keeps_the_section_flat() {
        let m = model(vec![
            row("1", BoardState::Working),
            row("2", BoardState::Working),
        ]);
        let lines = m.lines();
        assert!(
            !lines.iter().any(|l| matches!(l, BoardLine::Group(..))),
            "one route needs no header: {lines:?}"
        );
    }

    #[test]
    fn the_cursor_toggles_a_group_it_sits_on() {
        let mut rows = vec![row("1", BoardState::Ready), row("2", BoardState::Ready)];
        rows[1].route = Some("tally".into());
        let mut m = model(rows);
        m.selected = Some(group_row_id(BoardState::Ready, Some("tally")));
        let (state, route) = m.on_group().expect("cursor is on a group header");
        assert_eq!(state, BoardState::Ready);
        assert_eq!(route.as_deref(), Some("tally"));
        assert!(!m.is_group_collapsed(state, route.as_deref()));
        m.toggle_group(state, route.as_deref());
        assert!(m.is_group_collapsed(state, route.as_deref()));
        assert!(
            !m.lines().iter().any(|l| l.id() == "2"),
            "a folded group hides its rows"
        );
    }

    #[test]
    fn selection_survives_refresh_by_id() {
        let mut m = model(vec![
            row("1", BoardState::Ready),
            row("2", BoardState::Ready),
        ]);
        m.selected = Some("2".into());
        // A later frame with the same row keeps the cursor.
        m.set_rows(vec![
            row("1", BoardState::Ready),
            row("2", BoardState::Ready),
        ]);
        assert_eq!(m.selected.as_deref(), Some("2"));
        // A frame where the row left the board re-clamps.
        m.set_rows(vec![row("1", BoardState::Ready)]);
        assert_eq!(m.selected.as_deref(), Some("1"));
    }

    /// gh#55: the host rides on every board call as `targetDeviceId`, and a
    /// local board sends exactly the shape it always did.
    #[test]
    fn the_host_passthrough_is_added_only_when_the_board_is_elsewhere() {
        let local = host_params(None, serde_json::json!({ "taskId": "t" }));
        assert_eq!(local, serde_json::json!({ "taskId": "t" }));
        let remote = host_params(Some("box"), serde_json::json!({ "taskId": "t" }));
        assert_eq!(
            remote,
            serde_json::json!({ "taskId": "t", "targetDeviceId": "box" })
        );
    }

    #[test]
    fn f_cycles_routes_then_clears() {
        let mut rows = vec![row("a", BoardState::Ready), row("b", BoardState::Ready)];
        rows[1].route = Some("itsm-agent".into());
        let mut m = model(rows);
        assert!(m.cycle_filter().is_none());
        assert_eq!(m.filter, Filter::Route("itsm-agent".into()));
        assert!(m.cycle_filter().is_none());
        assert_eq!(m.filter, Filter::Route("offhand".into()));
        // The wrap: back to everything.
        assert!(m.cycle_filter().is_none());
        assert_eq!(m.filter, Filter::All);
    }

    #[test]
    fn f_in_the_focused_model_search_does_not_cycle_the_filter() {
        // Regression for gh#45: `f` typed into the model search must filter
        // models, not the board. While the search is focused the picker
        // consumes the plain key, so the frame's `f` → cycle_filter path never
        // runs and the board filter is untouched.
        let key = "f";
        assert!(
            dispatch_picker_owns_key(key, true),
            "the focused search owns the letter"
        );
        let mut m = model(vec![
            row("a", BoardState::Ready),
            row("b", BoardState::Ready),
        ]);
        let before = m.filter.clone();
        if !dispatch_picker_owns_key(key, true) {
            // The buggy fall-through: the frame handler runs cycle_filter.
            m.cycle_filter();
        }
        assert_eq!(m.filter, before, "the board filter did not change");
        assert_eq!(m.filter, Filter::All);

        // Control: with the search NOT focused, `f` reaches the frame handler
        // and cycles the filter as the shortcut intends.
        assert!(
            !dispatch_picker_owns_key(key, false),
            "frame-focused f still cycles"
        );
        let mut frame = model(vec![
            row("a", BoardState::Ready),
            row("b", BoardState::Ready),
        ]);
        frame.cycle_filter();
        assert_eq!(frame.filter, Filter::Route("offhand".into()));

        // Navigation keys are always the picker's, focused or not.
        assert!(dispatch_picker_owns_key("down", true));
        assert!(dispatch_picker_owns_key("down", false));
    }

    #[test]
    fn empty_board_filter_cycle_clears_and_says_so() {
        let mut m = model(vec![]);
        assert_eq!(
            m.cycle_filter(),
            Some("nothing on the board to filter".into())
        );
        assert_eq!(m.filter, Filter::All);
    }

    #[test]
    fn find_field_filters_live_and_closes() {
        let mut m = model(vec![row("a", BoardState::Ready)]);
        m.open_find();
        assert!(m.typing);
        m.find_type('a');
        assert_eq!(m.filter, Filter::Text("a".into()));
        m.accept_find();
        assert!(!m.typing);
        assert_eq!(m.filter, Filter::Text("a".into()));
        // Escape clears the query and closes the field.
        m.open_find();
        m.find_type('x');
        m.escape_find();
        assert!(!m.typing);
        assert_eq!(m.filter, Filter::All);
    }

    #[test]
    fn selection_skips_folded_sections_and_clamps() {
        let mut m = model(vec![
            row("1", BoardState::Ready),
            row("2", BoardState::Ready),
        ]);
        m.selected = Some("1".into());
        m.select_delta(1);
        assert_eq!(m.selected.as_deref(), Some("2"));
        // Selection cannot walk past the board's end.
        m.select_delta(5);
        assert_eq!(m.selected.as_deref(), Some("2"));
        // A filter that hides the selection re-clamps to a visible line.
        m.filter = Filter::Text("zzz".into());
        m.clamp_selection();
        assert_eq!(m.filter.empty_note().is_some(), true);
        // Every row filtered away → no lines left, and nothing is selected.
        assert!(m.selected.is_none());
    }

    #[test]
    fn section_header_ids_cannot_collide_with_task_ids() {
        for state in BoardState::SECTION_ORDER {
            let id = section_row_id(state);
            assert!(
                id.starts_with('\u{0}'),
                "NUL prefix keeps headers out of the task space"
            );
            assert_ne!(id, "anything");
        }
    }

    /// E6: the two right-hand columns say one thing each, and a row with
    /// nothing to say there says nothing rather than something else.
    #[test]
    fn the_row_columns_are_one_fact_each() {
        let now = Utc::now();
        // A ready row has no clock — and the column stays a column.
        let ready = row("r", BoardState::Ready);
        assert_eq!(time_cell(&ready, now), "");

        let mut working = row("w", BoardState::Working);
        working.started_at = Some(now.to_rfc3339());
        assert_eq!(time_cell(&working, now), "0s");

        let mut review = row("v", BoardState::Review);
        review.pr_number = Some(7);
        assert_eq!(time_cell(&review, now), "PR #7");
        review.pr_number = None;
        assert_eq!(time_cell(&review, now), "no PR");

        let mut failed = row("f", BoardState::Failed);
        failed.state = BoardState::Failed.as_str().into();
        assert_eq!(time_cell(&failed, now), "exited");

        // E13: history's column carries the agent that closed it.
        let mut done = row("d", BoardState::Done);
        done.state = BoardState::Done.as_str().into();
        done.runtime = Some("claude".into());
        assert_eq!(time_cell(&done, now), "claude");
    }

    /// The repo column is the repository a GitHub id names, and the space a
    /// Linear one runs in — never the owner, and never empty when the row
    /// knows either.
    #[test]
    fn the_repo_column_names_the_repository_or_the_space() {
        let mut gh = row("x", BoardState::Ready);
        gh.id = "gh:Florin-AS/tally#507".into();
        assert_eq!(repo_cell(&gh), "tally");

        let mut linear = row("y", BoardState::Ready);
        linear.id = "lin:TAL-218".into();
        linear.workspace = Some("tally-web".into());
        assert_eq!(repo_cell(&linear), "tally-web");

        let mut bare = row("z", BoardState::Ready);
        bare.id = "lin:TAL-9".into();
        bare.workspace = None;
        assert_eq!(repo_cell(&bare), "");
    }

    /// gh#132: every row is the same height, whatever the pointer is doing.
    /// The row is one line now (gh#176) and that line is a constant — which is
    /// the invariant, since a row that can grow is a row the list reflows
    /// around.
    #[test]
    fn a_row_cannot_change_height_under_the_pointer() {
        // The declared height IS the content's: one fixed line and its
        // padding. A row that declared less would clip the line; one that
        // declared more would drift from what it draws.
        assert_eq!(ROW_H, 32.0);
        assert_eq!(ROW_H, ROW_PAD_Y * 2.0 + ROW_LINE_H);
        // The line is exactly the action chip's height (E9), so a row that
        // draws a second verb is exactly as tall as one that does not.
        assert_eq!(ROW_LINE_H, 22.0);
    }

    /// gh#176's exit criterion, in the geometry it is a claim about: the list
    /// area the design review measured at the default 520px pane width showed
    /// ten rows, and shows fifteen now.
    #[test]
    fn fifteen_rows_fit_where_ten_did() {
        /// Two lines, the gap and the padding — the row gh#132 froze.
        const OLD_ROW_H: f32 = 47.0;
        const LIST_H: f32 = 480.0;
        assert_eq!((LIST_H / OLD_ROW_H).floor(), 10.0);
        assert_eq!((LIST_H / ROW_H).floor(), 15.0);
        // And the fifteen pixels the five extra rows came out of are exactly
        // the metadata line that a ready row held empty by contract.
        assert_eq!(OLD_ROW_H - ROW_H, 15.0);
    }

    /// The chips a row draws are the shared rule's, not a second copy of it —
    /// so the peek panel, the TUI's keys and the phone's sheet cannot drift
    /// into offering a row a verb the desktop does not.
    #[test]
    fn the_rows_chips_are_the_shared_action_set() {
        use comet_proto::view::board::RowAction;
        assert_eq!(
            board::row_actions(&row("r", BoardState::Ready)),
            vec![RowAction::Dispatch]
        );
        assert_eq!(
            board::row_actions(&row("w", BoardState::Working)),
            vec![RowAction::OpenChat, RowAction::Cancel]
        );
        // And every action this surface can draw has a colour and a stable
        // element-id fragment — a missing arm would be a panic at paint time.
        let theme = Theme::dark();
        for action in [
            RowAction::Dispatch,
            RowAction::Retry,
            RowAction::Cancel,
            RowAction::OpenChat,
            RowAction::OpenIssue,
            RowAction::OpenPr,
        ] {
            assert!(!action_key(action).is_empty());
            let _ = action_color(action, &theme);
        }
    }

    /// gh#176: one verb per row is drawn whether or not anything is hovering
    /// it, and it is the verb `enter` runs — the designation is the shared
    /// one, so the chip and the key cannot come to mean different things.
    #[test]
    fn the_visible_verb_is_the_one_enter_runs() {
        assert_eq!(
            board::primary_action(&row("r", BoardState::Ready)),
            Some(RowAction::Dispatch)
        );
        // The rest stay on hover, and together they are the whole set.
        let blocked = row("b", BoardState::Blocked);
        assert_eq!(board::primary_action(&blocked), Some(RowAction::OpenChat));
        assert_eq!(
            board::secondary_actions(&blocked),
            vec![RowAction::Retry, RowAction::Cancel]
        );
        // The footer says the same thing in words, from the same rule.
        assert_eq!(
            board::primary_action(&blocked).map(|a| format!("enter to {}", a.verb())),
            Some("enter to open chat".to_string())
        );
    }

    /// The header's words are the published ones, in this surface's case
    /// (gh#176) — the contract is the vocabulary, not the capitals.
    #[test]
    fn section_titles_are_the_shared_words_in_sentence_case() {
        for state in BoardState::SECTION_ORDER {
            let title = section_title(state);
            assert_eq!(
                title.to_uppercase(),
                if state == BoardState::Done {
                    "DONE TODAY".to_string()
                } else {
                    state.label().to_string()
                },
                "{state:?}"
            );
            assert!(title.starts_with(|c: char| c.is_uppercase()), "{title}");
        }
    }

    fn runtimes() -> Vec<BoardRuntime> {
        [
            ("claude-code", "Claude Code", HarnessId::ClaudeCode),
            ("opencode", "OpenCode", HarnessId::Opencode),
            ("codex", "Codex", HarnessId::Codex),
        ]
        .into_iter()
        .map(|(name, label, harness)| BoardRuntime {
            name: name.into(),
            label: label.into(),
            harness,
            unavailable: None,
        })
        .collect()
    }

    #[test]
    fn the_picker_defaults_to_the_routes_runtime() {
        let options = runtimes();
        assert_eq!(
            default_runtime_index(&options, Some("opencode")),
            1,
            "a route's canonical runtime is where the cursor starts"
        );
    }

    #[test]
    fn a_route_alias_lands_on_its_harnesss_canonical_entry() {
        let options = runtimes();
        // `claude` is config spelling for the claude-code harness; the picker
        // offers only canonical names, so the cursor starts at the first
        // (claude-code) rather than nowhere.
        assert_eq!(default_runtime_index(&options, Some("claude")), 0);
        assert_eq!(default_runtime_index(&options, None), 0);
    }

    #[test]
    fn an_unknown_route_runtime_falls_back_to_the_first_option() {
        let options = runtimes();
        assert_eq!(default_runtime_index(&options, Some("nonesuch")), 0);
    }

    /// The route's runtime is where the cursor starts even when the host cannot
    /// run it (gh#187) — that sentence is the thing worth reading. But a
    /// *fallback* never lands on a dead end.
    #[test]
    fn the_cursor_reports_the_routes_runtime_and_falls_back_to_a_live_one() {
        use comet_proto::view::board::RuntimeUnavailable;
        let mut options = runtimes();
        options[0].unavailable = Some(RuntimeUnavailable::SignedOut);
        options[1].unavailable = Some(RuntimeUnavailable::NotInstalled);

        assert_eq!(
            default_runtime_index(&options, Some("opencode")),
            1,
            "the route sends work there; saying so is the point"
        );
        assert_eq!(
            default_runtime_index(&options, None),
            2,
            "with nothing chosen, start on one that could actually run"
        );
        assert_eq!(default_runtime_index(&options, Some("nonesuch")), 2);
        assert!(!options[0].available() && options[2].available());
    }

    fn models() -> Vec<BoardModelInfo> {
        [
            ("opencode/big-pickle", "OpenCode Big Pickle"),
            ("opencode/deepseek-v4-flash", "Deepseek V4 Flash"),
        ]
        .into_iter()
        .map(|(id, label)| BoardModelInfo {
            id: id.into(),
            label: label.into(),
        })
        .collect()
    }

    #[test]
    fn the_default_row_highlights_the_harness_default_without_an_override() {
        let catalog = models();
        // The first catalog row is the harness default (gh#38: big-pickle); the
        // picker's row 0 IS that model — the row labels it, but dispatch sends
        // no override, so the route's behavior is unchanged.
        assert_eq!(catalog[0].id, "opencode/big-pickle");
        assert_eq!(override_model_id(&catalog, 0), None);
    }

    #[test]
    fn a_picked_model_is_the_override_and_the_effective_model() {
        let catalog = models();
        assert_eq!(
            catalog[1].id, "opencode/deepseek-v4-flash",
            "deepseek-v4-flash is selectable, one row past the default"
        );
        assert_eq!(
            override_model_id(&catalog, 1),
            Some("opencode/deepseek-v4-flash")
        );
    }

    #[test]
    fn an_out_of_range_highlight_never_sends_a_foreign_model() {
        let catalog = models();
        assert_eq!(catalog.get(99).map(|m| m.id.as_str()), None);
        assert_eq!(override_model_id(&catalog, 99), None);
    }

    #[test]
    fn model_search_matches_id_and_label_and_narrows() {
        let catalog = models();
        // An empty query shows the whole catalog in order, so row 0 stays the
        // harness default (no override).
        assert_eq!(filtered_model_indices(&catalog, ""), vec![0, 1]);
        assert_eq!(filtered_model_indices(&catalog, "  "), vec![0, 1]);
        // "deepseek" matches the id opencode/deepseek-v4-flash.
        assert_eq!(
            filtered_model_indices(&catalog, "deepseek"),
            vec![1],
            "an id substring match narrows to that model"
        );
        // A label match finds the model too.
        assert_eq!(
            filtered_model_indices(&catalog, "big"),
            vec![0],
            "a label substring match narrows to that model"
        );
        // No match → the list empties (nothing to dispatch blindly).
        assert!(filtered_model_indices(&catalog, "nonesuch").is_empty());
    }

    #[test]
    fn enter_dispatches_the_filtered_selection() {
        let catalog = models();
        // Type "deepseek" → the filter narrows to catalog row 1. The highlight
        // re-homes on the first match (the Edited event resets active to 0),
        // and that row maps to a real override — enter sends it.
        let filtered = filtered_model_indices(&catalog, "deepseek");
        assert_eq!(filtered, vec![1]);
        let active = 0; // what typing re-homes the highlight to
        assert_eq!(filtered.get(active), Some(&1));
        assert_eq!(
            override_model_id(&catalog, filtered[active]),
            Some("opencode/deepseek-v4-flash"),
            "enter dispatches the filtered selection as the override"
        );
        // An out-of-range highlight (nothing matches) dispatches nothing.
        let empty = filtered_model_indices(&catalog, "nonesuch");
        assert!(empty.get(0).is_none());
    }

    // ---- the account strip (gh#74) ----

    fn account(id: &str, email: &str, harness: HarnessId) -> AgentAccount {
        AgentAccount {
            id: id.into(),
            harness,
            email: Some(email.into()),
            plan_label: None,
            active: false,
            usage_windows: Vec::new(),
            display_name: None,
            organization: None,
            auth_kind: None,
            switchable: true,
            saved_at: None,
        }
    }

    fn accounts() -> Vec<AgentAccount> {
        vec![
            account("slot-ana", "ana@example.com", HarnessId::ClaudeCode),
            account("slot-box", "box@example.com", HarnessId::ClaudeCode),
            account("slot-cod", "cod@example.com", HarnessId::Codex),
        ]
    }

    #[test]
    fn the_strip_offers_only_logins_the_runtime_can_spend() {
        let all = accounts();
        let claude = accounts_for_harness(&all, Some(HarnessId::ClaudeCode));
        assert_eq!(
            claude.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            vec!["slot-ana", "slot-box"],
            "a codex slot cannot pay for a claude run"
        );
        assert_eq!(
            accounts_for_harness(&all, Some(HarnessId::Cursor)).len(),
            0,
            "a harness with nothing saved offers nothing but the default"
        );
        assert!(
            accounts_for_harness(&all, None).is_empty(),
            "before the runtime list lands there is no harness to filter by"
        );
    }

    #[test]
    fn row_zero_is_the_routes_account_and_sends_no_override() {
        let all = accounts();
        let options = accounts_for_harness(&all, Some(HarnessId::ClaudeCode));
        assert_eq!(override_account_id(&options, 0), None);
        assert_eq!(override_account_id(&options, 1), Some("slot-ana"));
        assert_eq!(override_account_id(&options, 2), Some("slot-box"));
        // An out-of-range highlight never sends someone else's login.
        assert_eq!(override_account_id(&options, 9), None);
    }

    #[test]
    fn an_account_chip_names_the_person_it_bills() {
        let mut anonymous = account("slot-x", "", HarnessId::ClaudeCode);
        anonymous.email = None;
        assert_eq!(
            account_label(&account(
                "slot-ana",
                "ana@example.com",
                HarnessId::ClaudeCode
            )),
            "ana@example.com"
        );
        anonymous.display_name = Some("Ana".into());
        assert_eq!(account_label(&anonymous), "Ana");
        anonymous.display_name = None;
        assert_eq!(
            account_label(&anonymous),
            "slot-x",
            "a slot with no name still has to be pickable"
        );
    }

    // ---- who pays (gh#101) ----

    /// A picker as a teammate sees it: the box's live login belongs to the
    /// owner, `ana` is at the keyboard, and the route names no account.
    fn shared_box_draft() -> DispatchDraft {
        let mut box_login = account("slot-box", "brede@tally.no", HarnessId::ClaudeCode);
        box_login.active = true;
        DispatchDraft {
            task_id: "gh:o/r#101".into(),
            identifier: "gh#101".into(),
            route_runtime: Some("claude-code".into()),
            runtimes: runtimes(),
            active_runtime: 0,
            runtime_error: None,
            catalogs: HashMap::new(),
            row: PickerRow::Account,
            active_model: 0,
            accounts: AccountCatalog::Ready(vec![
                box_login,
                account("slot-ana", "ana@example.com", HarnessId::ClaudeCode),
            ]),
            active_account: 0,
            route_account: None,
            viewer: Some("ana@example.com".into()),
        }
    }

    /// gh#101's exit criterion in the panel: **row 0** is the chip an
    /// enter-enter release lands on, and on a route with no account of its own
    /// it spends the box's login — the owner's. That is the row that has to say
    /// whose plan it charges.
    #[test]
    fn row_zero_says_when_the_route_default_charges_somebody_else() {
        let draft = shared_box_draft();
        assert_eq!(
            draft.bills(draft.route_account.as_deref()).as_deref(),
            Some("bills brede@tally.no"),
            "the route default resolves to the box's live login"
        );
        // A route that names the owner's slot outright reads the same way — the
        // slot id itself would have told the teammate nothing.
        let mut routed = shared_box_draft();
        routed.route_account = Some("slot-box".into());
        assert_eq!(
            routed.bills(routed.route_account.as_deref()).as_deref(),
            Some("bills brede@tally.no")
        );
        // Her own slot bills her, and says nothing.
        assert_eq!(draft.bills(Some("slot-ana")), None);
    }

    /// The warning is a comparison, not a decoration: without a signed-in user
    /// there is nothing to compare, and the panel accuses nobody.
    #[test]
    fn a_picker_that_cannot_tell_who_you_are_warns_about_nothing() {
        let mut anonymous = shared_box_draft();
        anonymous.viewer = None;
        assert_eq!(anonymous.bills(None), None);
        assert_eq!(anonymous.bills(Some("slot-box")), None);

        // Nor does it warn about a harness whose logins it has not seen — the
        // codex strip on a box with only Claude slots saved.
        let mut codex = shared_box_draft();
        codex.active_runtime = codex
            .runtimes
            .iter()
            .position(|r| r.harness == HarnessId::Codex)
            .expect("the catalog offers codex");
        assert_eq!(codex.bills(None), None);
    }

    /// What the confirm has to name when the host refuses under `require-own`:
    /// the account the *highlight* will actually spend, not the route's.
    #[test]
    fn the_payer_the_confirm_names_follows_the_highlight() {
        let mut draft = shared_box_draft();
        assert_eq!(draft.billed_to().as_deref(), Some("brede@tally.no"));
        // Highlight her own slot (row 0 is the route's, so row 2 is slot-ana).
        draft.active_account = 2;
        assert_eq!(draft.billed_to().as_deref(), Some("ana@example.com"));
    }

    /// The first send never consents — the guard exists to make somebody say
    /// it, and a picker that pre-filled `bill` would tick the box for them.
    #[test]
    fn a_release_carries_no_acknowledgement_until_the_host_asks_for_one() {
        assert_eq!(shared_box_draft().choice(None).bill, None);
    }
}
