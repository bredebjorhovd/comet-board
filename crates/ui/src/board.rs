//! The board panel (docs/BOARD.md §H10): the herdr task board as a citizen of
//! the desktop app — a right dock toggled with Cmd/Ctrl+Shift+B, fed by the
//! engine's `WatchBoard` stream.
//!
//! The *derivations* all live in `comet_proto::view::board` — the section
//! grouping, the `f`/`/` filter cycle, what each row says — so this panel
//! renders exactly the rows the TUI (`comet-tui`) renders, with the same
//! vocabulary. This module holds only the interactive state the derivations
//! need a home for: the rows as they streamed, the selection, the filter, the
//! folded sections, and the dispatch/cancel/open-chat verbs.
//!
//! - Sections in fixed order (blocked → working → ready → review → failed →
//!   done), empty ones omitted; `done` is bounded to today by the shared
//!   derivation;
//! - `enter` on a ready row dispatches it (`DispatchTask`); on a running row it
//!   opens that attempt's chat; on a section header it folds/unfolds;
//! - `f` cycles the routes on the board, `F` clears the filter, `/` opens the
//!   find field (live substring matching), `esc` closes the panel;
//! - the panel is lazy: no RPC until it is first opened, and the stream
//!   reconnects with a 2 s backoff if the engine drops it.

use std::collections::HashSet;
use std::time::Duration;

use chrono::Utc;
use gpui::{
    actions, AnyElement, App, Context, Entity, FocusHandle, Focusable as _, IntoElement,
    KeyDownEvent, MouseButton, Render, ScrollHandle, SharedString, Subscription, Task, Window,
    div, prelude::*, px,
};

use comet_proto::view::board::{self, BoardState, Filter, TaskRow};
use comet_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::icons::{self, icon};
use crate::motion;
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

/// A line the board body draws: a section header, or a task row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoardLine {
    Section(BoardState),
    Task(String),
}

impl BoardLine {
    /// The selection id this line answers to. Header ids carry a NUL prefix so
    /// they can never collide with a task id (same convention as the TUI).
    fn id(&self) -> String {
        match self {
            BoardLine::Section(state) => section_row_id(*state),
            BoardLine::Task(id) => id.clone(),
        }
    }
}

/// Selection id for a section header. The NUL prefix keeps it out of the task
/// id space.
fn section_row_id(state: BoardState) -> String {
    format!("\u{0}section:{}", state.as_str())
}

/// The accent a board state carries, matching the TUI's palette: blocked and
/// failed share red (the glyph tells them apart), working is amber, review
/// indigo, ready plain text, done dim.
fn state_color(state: BoardState, theme: &Theme) -> gpui::Hsla {
    match state {
        BoardState::Blocked | BoardState::Failed => theme.danger,
        BoardState::Working => theme.warning,
        BoardState::Review => theme.accent,
        BoardState::Ready => theme.text,
        BoardState::Done => theme.text_faint,
    }
}

/// The `[process exited]`-free reason a working/blocked row's metadata names
/// the runtime of — see [`board::row_metadata`] for the shared derivation this
/// trims the terminal padding off.
fn metadata(row: &TaskRow, now: chrono::DateTime<Utc>) -> String {
    board::row_metadata(row, false, 120, now).trim_end().to_string()
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

    /// Every line the body draws, in display order.
    pub fn lines(&self) -> Vec<BoardLine> {
        let mut out = Vec::new();
        for (state, rows) in self.sections() {
            out.push(BoardLine::Section(state));
            if self.collapsed.contains(&state) {
                continue;
            }
            out.extend(rows.iter().map(|row| BoardLine::Task(row.id.clone())));
        }
        out
    }

    /// How many task rows the filter lets through, for the header count.
    pub fn shown_tasks(&self) -> usize {
        self.sections().iter().map(|(_, rows)| rows.len()).sum()
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

    pub fn find_backspace(&mut self) {
        if let Filter::Text(q) = &mut self.filter
            && q.pop().is_none()
        {
            // Backspacing past the start closes the field. An empty field
            // filtering nothing is not a state worth being stuck in.
            self.clear_filter();
            return;
        }
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

/// The board dock. Lazy: no RPC until [`BoardPanel::set_open`] first runs.
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
    /// The board body's scroll position.
    scroll: ScrollHandle,
    /// A transient dispatch/cancel message for the footer.
    notice: Option<SharedString>,
    /// Keeps the elapsed counters on working/blocked rows live.
    _ticker: Task<()>,
    _observe: Subscription,
}

impl BoardPanel {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |this: &mut Self, _, cx| {
            if this.open && !this.started {
                this.ensure_watch(cx);
            }
        });
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let alive = this.update(cx, |panel, cx| {
                    // Keep the elapsed counters live only while the board is
                    // on screen and something is actually running.
                    if panel.open
                        && panel.model.rows.iter().any(|row| {
                            matches!(
                                row.state(),
                                BoardState::Working | BoardState::Blocked
                            )
                        })
                    {
                        cx.notify();
                    }
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
            scroll: ScrollHandle::new(),
            notice: None,
            _ticker: ticker,
            _observe: observe,
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus_handle.clone()
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

    fn spawn_watch(engine: EngineHandle, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let subscribed = engine
                    .client()
                    .subscribe(methods::WATCH_BOARD, serde_json::json!({}))
                    .await;
                match subscribed {
                    Ok(mut rx) => {
                        while let Some(value) = rx.recv().await {
                            let alive = this.update(cx, |panel, cx| {
                                panel.error = None;
                                match serde_json::from_value::<Vec<TaskRow>>(value) {
                                    Ok(rows) => {
                                        panel.model.set_rows(rows);
                                        cx.notify();
                                    }
                                    Err(err) => tracing::warn!(
                                        error = %err,
                                        "board: dropping malformed watch frame"
                                    ),
                                }
                            });
                            if alive.is_err() {
                                return;
                            }
                        }
                        // Stream ended (engine restart / reconnect): banner +
                        // retry, with the last content still visible.
                        if this
                            .update(cx, |panel, cx| {
                                panel.error = Some("Board stream interrupted — retrying".into());
                                cx.notify();
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(err) => {
                        if this
                            .update(cx, |panel, cx| {
                                panel.error = Some(format!("Board unavailable: {err}").into());
                                cx.notify();
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                cx.background_executor().timer(Duration::from_secs(2)).await;
            }
        })
    }

    // ---- verbs ----

    /// Release a ready task. The operator is dispatching, so there is no `via`
    /// — provenance is never fabricated (the same rule the TUI follows).
    fn dispatch(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(engine) = self.engine(cx) else {
            self.set_notice("Engine not connected", cx);
            return;
        };
        let Some(row) = self.model.task(id) else { return };
        if !row.dispatchable {
            self.set_notice(
                format!("{} has no route — it cannot be dispatched", row.identifier),
                cx,
            );
            return;
        }
        let identifier = row.identifier.clone();
        let task_id = row.id.clone();
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::DISPATCH_TASK, serde_json::json!({ "taskId": task_id }))
                .await;
            this.update(cx, |panel, cx| match result {
                Ok(value) => {
                    let chat = value
                        .get("chatId")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .unwrap_or_default();
                    panel.set_notice(
                        format!("Dispatched {identifier} — chat {chat} is on it"),
                        cx,
                    );
                }
                Err(err) => {
                    panel.set_notice(format!("Couldn't dispatch {identifier}: {err}"), cx);
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
        let Some(row) = self.model.task(id) else { return };
        let identifier = row.identifier.clone();
        let task_id = row.id.clone();
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::CANCEL_TASK, serde_json::json!({ "taskId": task_id }))
                .await;
            this.update(cx, |panel, cx| match result {
                Ok(_) => panel.set_notice(format!("Cancelled {identifier}"), cx),
                Err(err) => panel.set_notice(format!("Couldn't cancel {identifier}: {err}"), cx),
            })
            .ok();
        })
        .detach();
    }

    /// Jump to a running task's chat (herdr-board's `g`): the attempt's chat is
    /// where the work is, and comet's answer to a pane is a chat. The board
    /// gives way — the chat is the destination.
    fn open_chat(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.model.task(id) else { return };
        let Some(chat_id) = row.chat_id.clone() else {
            self.set_notice(
                format!("{} is running but has no chat to open", row.identifier),
                cx,
            );
            return;
        };
        self.state.update(cx, |s, cx| s.select_chat(Some(chat_id), cx));
        window.dispatch_action(Box::new(ToggleBoard), cx);
    }

    /// `enter` on the board: dispatch a ready task, fold a section header, or
    /// open a running task's chat.
    fn activate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = self.model.on_section() {
            self.model.toggle_collapsed(state);
            cx.notify();
            return;
        }
        let Some(row) = self.model.selected_task() else {
            return;
        };
        let (id, state) = (row.id.clone(), row.state());
        match state {
            BoardState::Ready => self.dispatch(&id, cx),
            BoardState::Working | BoardState::Blocked => self.open_chat(&id, window, cx),
            _ => {}
        }
    }

    // ---- find field ----

    fn open_find_field(&mut self, cx: &mut Context<Self>) {
        self.model.open_find();
        if self.find.is_none() {
            let input = cx.new(|cx| {
                ComposerInput::with_context("Search the board…", "PaletteSearch", cx)
            });
            let events = cx.subscribe(&input, |this: &mut Self, _, event, cx| {
                if matches!(event, ComposerInputEvent::Edited)
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
            cx.notify();
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

    fn render_header(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let typing = self.model.typing;
        let shown = self.model.shown_tasks();
        let filter_label = self.model.filter.label();
        let filter_active = self.model.filter.active();

        let title = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(7.0))
            .child(
                icon(icons::CHECKLIST)
                    .size(px(15.0))
                    .text_color(theme.text_muted.opacity(0.8)),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(SharedString::from("Board")),
            )
            .child(
                div()
                    .px(px(6.0))
                    .h(px(18.0))
                    .flex()
                    .items_center()
                    .rounded_full()
                    .bg(crate::theme::wash(0.08))
                    .text_size(px(10.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(shown.to_string())),
            );

        let mut header = div()
            .h(px(36.0))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(Theme::SPACE_LG))
            .border_b_1()
            .border_color(crate::theme::white_alpha(0.06))
            .child(title)
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
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(theme.border_strong)
                    .bg(crate::theme::wash(0.05))
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
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(crate::theme::wash(0.1)))
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
            // Filter cycle: click steps the routes like `f`; a clear chip
            // appears while a filter is active.
            let mut filter_chip = div()
                .id("board-filter")
                .h(px(24.0))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(5.0))
                .px(px(8.0))
                .rounded(px(6.0))
                .cursor_pointer()
                .bg(motion::hover_blend(
                    "board-filter",
                    crate::theme::wash(if filter_active { 0.09 } else { 0.0 }),
                    crate::theme::wash(0.14),
                ))
                .on_hover(motion::hover_listener("board-filter"))
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(message) = this.model.cycle_filter() {
                        this.set_notice(message, cx);
                    }
                    cx.notify();
                }))
                .text_size(px(11.0));
            let label: SharedString = filter_label
                .clone()
                .map(Into::into)
                .unwrap_or_else(|| "Filter".into());
            let label_color = if filter_active {
                theme.text
            } else {
                theme.text_muted.opacity(0.7)
            };
            filter_chip = filter_chip
                .child(icon(icons::TUNING).size(px(13.0)).text_color(label_color))
                .child(
                    div()
                        .max_w(px(150.0))
                        .truncate()
                        .text_color(label_color)
                        .child(label),
                );
            header = header.child(filter_chip);
            if filter_active {
                let clear_id = "board-filter-clear";
                header = header.child(
                    div()
                        .id(clear_id)
                        .size(px(24.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.0))
                        .cursor_pointer()
                        .hover(|s| s.bg(crate::theme::wash(0.1)))
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.model.clear_filter();
                            cx.notify();
                        }))
                        .child(
                            icon(icons::CLOSE_CIRCLE)
                                .size(px(14.0))
                                .text_color(theme.text_muted.opacity(0.8)),
                        ),
                );
            }
            // `/` find.
            header = header.child(
                div()
                    .id("board-find")
                    .size(px(24.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(crate::theme::wash(0.1)))
                    .on_click(cx.listener(|this, _, _, cx| this.open_find_field(cx)))
                    .child(
                        icon(icons::MAGNIFER)
                            .size(px(14.0))
                            .text_color(theme.text_muted.opacity(0.8)),
                    ),
            );
            // Close the dock.
            header = header.child(
                div()
                    .id("board-close")
                    .size(px(24.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(crate::theme::wash(0.1)))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(ToggleBoard), cx);
                    })
                    .child(
                        icon(icons::CLOSE)
                            .size(px(13.0))
                            .text_color(theme.text_muted.opacity(0.7)),
                    ),
            );
        }

        header.into_any_element()
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
            .text_size(px(12.0))
            .text_color(theme.text_faint)
            .child(text)
            .into_any_element()
    }

    /// A section header: glyph + uppercase label + the folded count where the
    /// rows would be (comet's board language — see the TUI renderer).
    fn render_section(
        &mut self,
        state: BoardState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let selected = self.model.selected.as_deref() == Some(section_row_id(state).as_str());
        let folded = self.model.is_collapsed(state);
        let len = self.model.section_len(state);
        let fade_key = format!("board-section-{}", state.as_str());
        let color = state_color(state, &theme);

        let el = div()
            .id(SharedString::from(format!(
                "board-section-{}",
                state.as_str()
            )))
            .h(px(30.0))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .px(px(Theme::SPACE_LG))
            .cursor_pointer()
            // Hover brightens from wherever the row rests — a selected section
            // never darkens toward a weaker wash.
            .bg(motion::hover_blend(
                &fade_key,
                selected_bg(selected),
                if selected {
                    crate::theme::wash(0.18)
                } else {
                    crate::theme::wash(0.06)
                },
            ))
            .on_hover(motion::hover_listener(&fade_key))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.model.toggle_collapsed(state);
                cx.notify();
            }))
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(color)
                    .child(SharedString::from(state.glyph())),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(11.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(if selected { theme.text } else { theme.text_muted })
                    .child(SharedString::from(if state == BoardState::Done {
                        "DONE TODAY".to_string()
                    } else {
                        state.label().to_string()
                    })),
            )
            .child(div().flex_1())
            .when(folded, |el| {
                el.child(
                    div()
                        .px(px(7.0))
                        .h(px(18.0))
                        .flex()
                        .items_center()
                        .rounded_full()
                        .bg(crate::theme::wash(0.08))
                        .text_size(px(10.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from(format!("{len} hidden"))),
                )
            })
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(theme.text_faint.opacity(0.7))
                    .child(SharedString::from(if folded {
                        "expand".to_string()
                    } else {
                        "collapse".to_string()
                    })),
            );

        el.into_any_element()
    }

    /// One task row: glyph + identifier + title on the first line, the shared
    /// metadata underneath, and a state action on hover/selection.
    #[allow(clippy::too_many_arguments)]
    fn render_task(
        &mut self,
        row: &TaskRow,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let state = row.state();
        let color = state_color(state, &theme);
        let fade_key = format!("board-row-{}", row.id);
        let hovered = motion::hover_t(&fade_key) > 0.5;
        let now = Utc::now();
        let meta = metadata(row, now);
        let id = row.id.clone();
        let select_id = id.clone();
        let open_id = id.clone();
        let identifier = row.identifier.clone();
        let title = row.title.clone();

        let actions = self.render_row_actions(row, selected || hovered, cx);

        div()
            .id(SharedString::from(format!("board-row-{}", row.id)))
            .h(px(44.0))
            .flex_none()
            .flex()
            .flex_col()
            .justify_center()
            .gap(px(2.0))
            .px(px(Theme::SPACE_LG))
            .cursor_pointer()
            .bg(motion::hover_blend(
                &fade_key,
                selected_bg(selected),
                if selected {
                    crate::theme::wash(0.18)
                } else {
                    crate::theme::wash(0.05)
                },
            ))
            .on_hover(motion::hover_listener(&fade_key))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.model.selected = Some(select_id.clone());
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    if event.click_count == 2 {
                        this.model.selected = Some(open_id.clone());
                        match this.model.task(&open_id).map(|r| r.state()) {
                            Some(BoardState::Ready) => this.dispatch(&open_id, cx),
                            Some(BoardState::Working | BoardState::Blocked) => {
                                this.open_chat(&open_id, window, cx)
                            }
                            _ => {}
                        }
                    }
                }),
            )
            // Line 1: glyph + identifier + title, then the actions.
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(7.0))
                    .child(
                        div()
                            .flex_none()
                            .w(px(12.0))
                            .text_size(px(11.0))
                            .text_color(color)
                            .child(SharedString::from(state.glyph())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .font_family(theme.font_mono.clone())
                            .text_size(px(11.0))
                            .text_color(if state == BoardState::Done {
                                theme.text_faint
                            } else {
                                theme.text_muted
                            })
                            .child(SharedString::from(identifier.clone())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.0))
                            .text_color(if state == BoardState::Done {
                                theme.text_faint
                            } else if selected {
                                theme.text
                            } else {
                                theme.text_muted.opacity(0.9)
                            })
                            .child(SharedString::from(title.clone())),
                    )
                    .child(actions),
            )
            // Line 2: the shared metadata.
            .when(!meta.is_empty(), |el| {
                el.child(
                    div()
                        .w_full()
                        .pl(px(19.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .text_size(px(10.5))
                        .text_color(theme.text_faint.opacity(0.85))
                        .truncate()
                        .child(SharedString::from(meta)),
                )
            })
            .into_any_element()
    }

    /// The row's state action, revealed on hover or selection: dispatch for a
    /// ready task, cancel (with a separate open-chat affordance) for a running
    /// one, and "Open PR" for a review waiting on you.
    fn render_row_actions(
        &mut self,
        row: &TaskRow,
        visible: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !visible {
            return gpui::Empty.into_any_element();
        }
        let theme = Theme::of(cx).clone();
        let id = row.id.clone();
        let state = row.state();
        let dispatch_id = id.clone();
        let open_id = id.clone();
        let cancel_id = id.clone();
        let pr_url = row.pr_url.clone();
        let chip = |key: String, label: &'static str, color: gpui::Hsla| {
            div()
                .id(key)
                .flex_none()
                .h(px(20.0))
                .px(px(8.0))
                .rounded(px(5.0))
                .bg(crate::theme::wash(0.12))
                .flex()
                .items_center()
                .text_size(px(10.5))
                .text_color(color)
                .hover(|s| s.bg(crate::theme::wash(0.18)))
                .child(SharedString::from(label))
        };
        match state {
            BoardState::Ready if row.dispatchable => chip(
                format!("board-dispatch-{id}"),
                "Dispatch",
                theme.text,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.dispatch(&dispatch_id, cx);
            }))
            .into_any_element(),
            BoardState::Working | BoardState::Blocked => div()
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .child(
                    chip(format!("board-open-{id}"), "Open", theme.text_muted)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.open_chat(&open_id, window, cx);
                        })),
                )
                .child(
                    chip(format!("board-cancel-{id}"), "Cancel", theme.danger)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.cancel(&cancel_id, cx);
                        })),
                )
                .into_any_element(),
            BoardState::Review if pr_url.is_some() => {
                let url = pr_url.clone().unwrap_or_default();
                chip(format!("board-pr-{id}"), "Open PR", theme.accent)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.open_pr_url(&url, cx);
                    }))
                    .into_any_element()
            }
            _ => gpui::Empty.into_any_element(),
        }
    }

    fn open_pr_url(&mut self, url: &str, cx: &mut Context<Self>) {
        cx.open_url(url);
    }

    /// The footer: a transient dispatch/cancel message owns it until it
    /// expires, then the board's key hints take over.
    fn render_footer(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let has_notice = self.notice.is_some();
        let notice = self.notice.clone();
        let typing = self.model.typing;
        let filter_active = self.model.filter.active();
        let on_section = self.model.on_section().is_some();
        let selected_task = self.model.selected_task().map(|r| r.state());

        let content: SharedString = if let Some(notice) = notice {
            notice
        } else if typing {
            "enter to keep the filter · esc to clear".into()
        } else {
            let mut hints: Vec<&str> = Vec::new();
            if on_section {
                hints.push("click to fold/unfold");
            }
            match selected_task {
                Some(BoardState::Ready) => hints.push("enter to dispatch"),
                Some(BoardState::Working | BoardState::Blocked) => hints.push("enter to open chat"),
                _ => {}
            }
            hints.push("f filter · / find");
            if filter_active {
                hints.push("F clears");
            }
            hints.push("esc close");
            hints.join(" · ").into()
        };

        div()
            .h(px(26.0))
            .flex_none()
            .flex()
            .items_center()
            .px(px(Theme::SPACE_LG))
            .border_t_1()
            .border_color(crate::theme::white_alpha(0.06))
            .text_size(px(10.5))
            .text_color(if has_notice {
                theme.warning
            } else {
                theme.text_faint.opacity(0.8)
            })
            .child(content)
            .into_any_element()
    }
}

fn selected_bg(selected: bool) -> gpui::Hsla {
    if selected {
        crate::theme::glass_selected_bg()
    } else {
        crate::theme::wash(0.0)
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
                        BoardLine::Section(state) => self.render_section(*state, cx),
                        BoardLine::Task(id) => {
                            let selected =
                                self.model.selected.as_deref() == Some(id.as_str());
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
            .key_context("Board")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window: &mut Window, cx| {
                    // Clicks on the board body re-arm keyboard navigation; a
                    // click inside the open find field must NOT steal focus
                    // from the input (typing owns it).
                    if !this.model.typing {
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
                        .text_size(px(11.0))
                        .text_color(theme.warning)
                        .child(message),
                )
            })
            .child(body)
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
            pr_url: None,
            pr_number: None,
            branch: Some("board/gh-x".into()),
            dispatched_by: None,
            dispatched_by_chat: None,
            last_outcome: None,
            last_outcome_at: None,
            attempts: 0,
            reopened: 0,
            updated_at: "2026-08-01T11:00:00Z".into(),
            started_at: None,
        }
    }

    fn model(rows: Vec<TaskRow>) -> BoardModel {
        let mut m = BoardModel::new();
        m.set_rows(rows);
        m
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
    fn selection_survives_refresh_by_id() {
        let mut m = model(vec![row("1", BoardState::Ready), row("2", BoardState::Ready)]);
        m.selected = Some("2".into());
        // A later frame with the same row keeps the cursor.
        m.set_rows(vec![row("1", BoardState::Ready), row("2", BoardState::Ready)]);
        assert_eq!(m.selected.as_deref(), Some("2"));
        // A frame where the row left the board re-clamps.
        m.set_rows(vec![row("1", BoardState::Ready)]);
        assert_eq!(m.selected.as_deref(), Some("1"));
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
    fn empty_board_filter_cycle_clears_and_says_so() {
        let mut m = model(vec![]);
        assert_eq!(m.cycle_filter(), Some("nothing on the board to filter".into()));
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
        let mut m = model(vec![row("1", BoardState::Ready), row("2", BoardState::Ready)]);
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
            assert!(id.starts_with('\u{0}'), "NUL prefix keeps headers out of the task space");
            assert_ne!(id, "anything");
        }
    }

    #[test]
    fn metadata_reuses_the_shared_derivation() {
        let now = Utc::now();
        let ready = row("r", BoardState::Ready);
        assert_eq!(metadata(&ready, now), "offhand");
        let mut w = row("w", BoardState::Working);
        w.started_at = Some(now.to_rfc3339());
        assert!(metadata(&w, now).contains("claude-code"));
        assert!(metadata(&w, now).contains("ws:offhand"));
        let mut rev = row("v", BoardState::Review);
        rev.pr_number = Some(7);
        assert_eq!(metadata(&rev, now), "PR #7 · waiting on you");
        let mut f = row("f", BoardState::Failed);
        f.state = BoardState::Failed.as_str().into();
        assert_eq!(metadata(&f, now), "pane exited without completing");
    }
}
