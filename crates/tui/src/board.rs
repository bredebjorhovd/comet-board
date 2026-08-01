//! The board pane's view model: what is on the board, and what the keys act on.
//!
//! The *derivations* live in `comet_proto::view::board` — section grouping, the
//! `f`/`/` filter cycle, what each row says. This module holds only the
//! interactive state the derivations need a home for: the rows as they last
//! streamed, the selection, the filter, the folded sections, the scroll, and
//! the actions that move all of those. The gpui app will grow the same pane
//! later without re-deriving any of it, because nothing here recomputes a rule
//! that proto already owns.

use std::collections::HashSet;

use chrono::Utc;
use comet_proto::view::board::{self, BoardState, Filter, TaskRow};

/// A line the board body draws: a section header, or a task row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoardRow {
    Section(BoardState),
    Task(String),
}

impl BoardRow {
    /// The selection id this line answers to. Header ids carry a NUL prefix so
    /// they can never collide with a task id.
    pub fn id(&self) -> String {
        match self {
            BoardRow::Section(state) => section_row_id(*state),
            BoardRow::Task(id) => id.clone(),
        }
    }
}

/// Selection id for a section header. The NUL prefix keeps it out of the task
/// id space.
pub fn section_row_id(state: BoardState) -> String {
    format!("\u{0}section:{}", state.as_str())
}

pub struct Board {
    /// Rows as the engine last streamed them, in board order.
    pub rows: Vec<TaskRow>,
    pub filter: Filter,
    /// The `/` field is open and taking keys. Implies [`Filter::Text`].
    pub typing: bool,
    pub selected: Option<String>,
    /// Sections folded away. `done` starts folded: it is history, and the rest
    /// is the queue.
    pub collapsed: HashSet<BoardState>,
    /// First body line on screen; rows below the fold are otherwise
    /// unreachable, which on a short pane is most of the board.
    pub scroll: usize,
    /// Height of the last render, for paging and scroll clamping.
    pub height: usize,
}

impl Default for Board {
    fn default() -> Self {
        Self::new()
    }
}

impl Board {
    pub fn new() -> Self {
        Self {
            rows: Vec::new(),
            filter: Filter::All,
            typing: false,
            selected: None,
            collapsed: HashSet::from([BoardState::Done]),
            scroll: 0,
            height: 1,
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
    pub fn lines(&self) -> Vec<BoardRow> {
        let mut out = Vec::new();
        for (state, rows) in self.sections() {
            out.push(BoardRow::Section(state));
            if self.is_collapsed(state) {
                continue;
            }
            out.extend(rows.iter().map(|row| BoardRow::Task(row.id.clone())));
        }
        out
    }

    /// How many task rows the filter lets through, for the count beside the
    /// `/` field.
    pub fn shown_tasks(&self) -> usize {
        self.sections().iter().map(|(_, rows)| rows.len()).sum()
    }

    pub fn task(&self, id: &str) -> Option<&TaskRow> {
        self.rows.iter().find(|row| row.id == id)
    }

    /// The selected task, or `None` — including when the cursor is on a
    /// section header, which is a line but not a task. A filtered-away row is
    /// **not** selected, however the cursor came to be on it.
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

    /// The first row worth landing on: a task, or the first line if the board
    /// has nothing but headers.
    pub fn first_task(&self) -> Option<String> {
        self.lines()
            .into_iter()
            .find_map(|line| match line {
                BoardRow::Task(id) => Some(id),
                BoardRow::Section(_) => None,
            })
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

    /// Page the selection, so `PageUp`/`PageDown` move a screen at a time.
    pub fn select_page(&mut self, delta: isize) {
        self.select_delta(delta.saturating_mul(self.height.max(1) as isize));
    }

    pub fn select_top(&mut self) {
        self.selected = self.lines().into_iter().next().map(|line| line.id());
    }

    pub fn select_bottom(&mut self) {
        self.selected = self.lines().into_iter().last().map(|line| line.id());
    }

    /// `f` — the next position, then back to all.
    ///
    /// No input mode, no cursor, nothing to escape: one key, and one more press
    /// always gets you further out. Wrapping past the last position lands on
    /// everything. Returns a message to flash when the cycle is empty (an empty
    /// board has nothing to say about routes).
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
    /// cursor being filtered away is the ordinary case, not an edge one.
    fn clamp_selection(&mut self) {
        let ids = self.lines();
        let keep = self
            .selected
            .as_deref()
            .is_some_and(|id| ids.iter().any(|line| line.id() == id));
        if !keep {
            self.selected = self.first_task().or_else(|| ids.first().map(|line| line.id()));
        }
    }

    /// Clamp the scroll so the selection is inside the window, moving as
    /// little as possible. Called by the renderer with the real pane height.
    pub fn ensure_visible(&mut self, height: usize) {
        self.height = height.max(1);
        let lines = self.lines();
        let max = lines.len().saturating_sub(height);
        if max == 0 {
            self.scroll = 0;
            return;
        }
        let Some(ix) = self
            .selected
            .as_deref()
            .and_then(|id| lines.iter().position(|line| line.id() == id))
        else {
            self.scroll = self.scroll.min(max);
            return;
        };
        if ix < self.scroll {
            self.scroll = ix;
        } else if ix >= self.scroll + height {
            self.scroll = ix + 1 - height;
        }
        self.scroll = self.scroll.min(max);
    }

    /// What to say when the filter has hidden every row.
    pub fn empty_note(&self) -> Option<String> {
        self.filter.empty_note()
    }
}
