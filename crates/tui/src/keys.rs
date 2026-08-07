//! Key → [`Action`] mapping, kept pure so the whole keymap is unit-testable
//! without a terminal.
//!
//! The binding style is "modeless with a text field": navigation keys are
//! single letters everywhere *except* the composer, where they are literal
//! characters and navigation moves to modifiers. That's the same split every
//! chat TUI converges on, and it's why [`map`] takes the focus — the same
//! `KeyEvent` means different things depending on where the caret is.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Which pane has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    /// The board pane — replaces the main pane, not a third column.
    Board,
    Transcript,
    Composer,
}

impl Focus {
    /// Tab order.
    pub fn next(self) -> Self {
        match self {
            Focus::Sidebar => Focus::Board,
            Focus::Board => Focus::Transcript,
            Focus::Transcript => Focus::Composer,
            Focus::Composer => Focus::Sidebar,
        }
    }

    pub fn previous(self) -> Self {
        match self {
            Focus::Sidebar => Focus::Composer,
            Focus::Transcript => Focus::Board,
            Focus::Composer => Focus::Transcript,
            Focus::Board => Focus::Sidebar,
        }
    }
}

/// Everything a key press can ask the app to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Leave the viewport. The engine keeps running.
    Quit,
    ToggleHelp,
    /// Scroll the help screen, which is longer than a pane.
    HelpScroll(isize),
    CloseOverlay,
    /// Open the context menu for the row under the cursor.
    ContextMenu,
    /// Open the model picker for the open session.
    PickModel,
    /// Open the branch picker (drafts only).
    PickRef,
    /// Open the "run in" picker (drafts only).
    PickCheckout,
    /// Open the effort picker for the active model.
    PickReasoning,
    /// Move the highlight in the composer's `/` skill picker (gh#134).
    SkillStep(isize),
    /// Complete the highlighted skill into the prompt.
    SkillAccept,
    /// Close the `/` picker, leaving the typed text alone.
    SkillDismiss,
    /// Move the selection inside a floating panel.
    OverlayStep(isize),
    /// Activate the floating panel's selection.
    OverlayConfirm,
    /// A keystroke aimed at a floating panel's text input.
    OverlayEdit(Edit),
    Focus(Focus),
    FocusNext,
    FocusPrevious,
    ToggleSidebar,
    /// Force an immediate reconnect attempt instead of waiting out the backoff.
    Reconnect,

    // Sidebar
    ListUp,
    ListDown,
    ListTop,
    ListBottom,
    /// Enter on a sidebar row.
    Open,
    NewSession,
    /// Archive or unarchive the session under the cursor.
    ToggleArchive,
    /// Show or hide archived sessions. Without this, an archived row is
    /// invisible and therefore impossible to unarchive.
    ToggleShowArchived,

    /// Select the tab `n` places away (wrapping).
    CycleTab(isize),
    /// Select the 1-based tab.
    SelectTab(usize),

    // Transcript
    ScrollUp(u16),
    ScrollDown(u16),
    PageUp,
    PageDown,
    ScrollTop,
    ScrollBottom,

    // Composer
    Send,
    Interrupt,
    Edit(Edit),

    // Board
    /// `B` — swap the main pane for the task board, and back.
    ToggleBoard,
    BoardUp,
    BoardDown,
    BoardPage(isize),
    BoardTop,
    BoardBottom,
    /// `enter` on the board: dispatch a ready task, fold a section header, or
    /// open a running task's chat.
    BoardEnter,
    /// `R` — retry the selected row: a blocked row's live attempt is replaced,
    /// a failed or ready one is released afresh (gh#73).
    BoardRetry,
    /// `f` — the next route filter, then no route, then all.
    BoardCycleFilter,
    /// `/` — open the find field.
    BoardFind,
    /// `F` — clear whichever filter is on.
    BoardClearFilter,
    /// `d` — step to the next board host: automatic, then this device, then
    /// each registered device, then back to automatic (gh#55).
    BoardCycleHost,
    /// A keystroke aimed at the board's find field.
    BoardFindType(char),
    BoardFindBackspace,
    /// `enter` on the find field: keep the query, close the field.
    BoardFindAccept,
    /// `esc` on the find field: clear it and close it.
    BoardFindEscape,
    /// `esc` / `h` / `B` from the board: back to the chat.
    BoardClose,
    /// `space` — open the selected row for reading, or shut it again (gh#132).
    /// A row is a door: `enter` releases, this one inspects.
    BoardPeek,
    /// `j`/`k` while a row is open: scroll its body.
    BoardPeekScroll(isize),
}

/// A composer mutation. Separated from [`Action`] so the app can apply the whole
/// family with one `match` on the buffer.
#[derive(Debug, Clone, PartialEq)]
pub enum Edit {
    Insert(char),
    Paste(String),
    Newline,
    Backspace,
    Delete,
    DeleteWordBack,
    DeleteToLineStart,
    DeleteToLineEnd,
    Left,
    Right,
    WordLeft,
    WordRight,
    Up,
    Down,
    Home,
    End,
    BufferStart,
    BufferEnd,
}

/// Map a key press. `overlay` is true while the help overlay is up, which
/// swallows everything but dismissal.
///
/// `composer_empty` decides Ctrl-D: on an empty prompt it means "I'm done"
/// (quit, as in a shell); with text it would be a destructive surprise, so it
/// is ignored.
pub fn map(focus: Focus, overlay: bool, composer_empty: bool, key: KeyEvent) -> Option<Action> {
    map_with(focus, overlay, composer_empty, false, false, false, false, key)
}

/// Like [`map`], plus whether the board's `/` field is open and swallowing the
/// printable keys, whether a board row is open for reading (gh#132, which owns
/// the keyboard the way the help screen does), whether the overlay up is the
/// help screen (whose list is longer than a pane and scrolls with `j`/`k`), and
/// whether the composer's `/` skill picker is showing rows (gh#134).
///
/// Split out so the event loop can pass the live flags without the test suite
/// having to repeat them everywhere. `map` keeps the three-flag shape.
// Five positional bools is past the point where a struct would read better —
// they are one keypress's live state, not an API, but the next flag added here
// should bundle them rather than make it six.
#[allow(clippy::too_many_arguments)]
pub fn map_with(
    focus: Focus,
    overlay: bool,
    composer_empty: bool,
    board_typing: bool,
    board_peek: bool,
    help: bool,
    skill_menu: bool,
    key: KeyEvent,
) -> Option<Action> {
    // Terminals with the kitty keyboard protocol report releases too; acting on
    // both would double every keystroke.
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    if overlay {
        return map_overlay(ctrl, alt, key, help);
    }
    if focus == Focus::Board {
        return map_board(board_typing, board_peek, key);
    }
    // The `/` picker takes the navigation keys ahead of everything else in the
    // composer — including Tab, which is a pane switch anywhere else. Typing
    // falls through untouched: the picker filters on what you write, so it must
    // never be the thing that swallows a keystroke.
    if skill_menu
        && focus == Focus::Composer
        && let Some(action) = map_skill_menu(key)
    {
        return Some(action);
    }

    // ---- bindings that mean the same thing in every pane ----
    match key.code {
        KeyCode::Char('c') if ctrl => return Some(Action::Quit),
        KeyCode::Char('x') if ctrl => return Some(Action::Interrupt),
        KeyCode::Char('b') if ctrl => return Some(Action::ToggleSidebar),
        // Composer chips: Alt-chords so they work mid-prompt, which is exactly
        // when you want them — the choices belong to the message you are about
        // to send.
        KeyCode::Char('m') if alt => return Some(Action::PickModel),
        KeyCode::Char('r') if alt => return Some(Action::PickRef),
        KeyCode::Char('w') if alt => return Some(Action::PickCheckout),
        KeyCode::Char('e') if alt => return Some(Action::PickReasoning),
        KeyCode::Tab => return Some(Action::FocusNext),
        KeyCode::BackTab => return Some(Action::FocusPrevious),
        // Paging the transcript from anywhere, including mid-prompt: reading
        // back while composing is the common case.
        KeyCode::PageUp => return Some(Action::PageUp),
        KeyCode::PageDown => return Some(Action::PageDown),
        // Tab strip. Alt-chords because they are unbound in every pane,
        // including the composer, where a bare digit is text.
        KeyCode::Left if alt => return Some(Action::CycleTab(-1)),
        KeyCode::Right if alt => return Some(Action::CycleTab(1)),
        KeyCode::Char(ch) if alt && ch.is_ascii_digit() && ch != '0' => {
            return Some(Action::SelectTab(ch as usize - '0' as usize));
        }
        _ => {}
    }

    match focus {
        Focus::Composer => map_composer(ctrl, alt, composer_empty, key),
        Focus::Sidebar => map_sidebar(ctrl, shift, key),
        Focus::Transcript => map_transcript(ctrl, shift, key),
        Focus::Board => unreachable!("board focus is routed to map_board"),
    }
}

/// Keys while the board pane owns the keyboard.
///
/// `typing` is true while the `/` field is open: then the field takes every
/// printable key and the navigation set shrinks to what closes or accepts it —
/// the same shape as the composer, because the field is a line of text.
fn map_board(typing: bool, peek: bool, key: KeyEvent) -> Option<Action> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    if typing {
        return match key.code {
            KeyCode::Esc => Some(Action::BoardFindEscape),
            KeyCode::Enter => Some(Action::BoardFindAccept),
            KeyCode::Backspace => Some(Action::BoardFindBackspace),
            KeyCode::Char(ch) if !ctrl && !alt => Some(Action::BoardFindType(ch)),
            _ => None,
        };
    }
    // An open row owns the keyboard, as the help screen does: it covers the
    // list, so a `j` that walked a selection nobody can see would be a key
    // press with no visible effect.
    //
    // `enter` is the exception, deliberately (gh#132): the detail is for
    // reading and must never become a step on the way to a release, so the
    // dispatch key still works from inside it. Everything else is swallowed —
    // a stray `f` must not cycle a filter behind a panel.
    if peek {
        return match key.code {
            KeyCode::Esc | KeyCode::Char(' ') => Some(Action::BoardPeek),
            KeyCode::Enter => Some(Action::BoardEnter),
            KeyCode::Char('j') | KeyCode::Down => Some(Action::BoardPeekScroll(1)),
            KeyCode::Char('k') | KeyCode::Up => Some(Action::BoardPeekScroll(-1)),
            KeyCode::PageDown => Some(Action::BoardPeekScroll(10)),
            KeyCode::PageUp => Some(Action::BoardPeekScroll(-10)),
            KeyCode::Char('c') if ctrl => Some(Action::Quit),
            KeyCode::Char('?') => Some(Action::ToggleHelp),
            _ => None,
        };
    }
    match key.code {
        KeyCode::Char('c') if ctrl => Some(Action::Quit),
        KeyCode::Char('b') if ctrl => Some(Action::ToggleSidebar),
        KeyCode::Tab => Some(Action::FocusNext),
        KeyCode::BackTab => Some(Action::FocusPrevious),
        KeyCode::Char('q') => Some(Action::Quit),
        KeyCode::Char('?') => Some(Action::ToggleHelp),
        KeyCode::Char('r') if !ctrl => Some(Action::Reconnect),
        KeyCode::Char('B') | KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left => {
            Some(Action::BoardClose)
        }
        KeyCode::Char('f') => Some(Action::BoardCycleFilter),
        KeyCode::Char('/') => Some(Action::BoardFind),
        KeyCode::Char('F') => Some(Action::BoardClearFilter),
        KeyCode::Char('d') if !ctrl => Some(Action::BoardCycleHost),
        // `r` is reconnect everywhere, including here; retry takes the shifted
        // spelling, as `F` does beside `f`.
        KeyCode::Char('R') => Some(Action::BoardRetry),
        // The door (gh#132). `enter` releases and must keep doing so from the
        // list, so inspecting a row gets its own key.
        KeyCode::Char(' ') => Some(Action::BoardPeek),
        KeyCode::Enter => Some(Action::BoardEnter),
        KeyCode::Char('j') | KeyCode::Down => Some(Action::BoardDown),
        KeyCode::Char('k') | KeyCode::Up => Some(Action::BoardUp),
        KeyCode::Char('g') if !ctrl => Some(Action::BoardTop),
        KeyCode::Char('G') => Some(Action::BoardBottom),
        KeyCode::Home => Some(Action::BoardTop),
        KeyCode::End => Some(Action::BoardBottom),
        KeyCode::PageUp => Some(Action::BoardPage(-1)),
        KeyCode::PageDown => Some(Action::BoardPage(1)),
        _ => None,
    }
}

/// Keys while a floating panel is up. It owns the keyboard: navigation moves
/// inside it, Esc dismisses, and printable characters go to its text input if
/// it has one — otherwise they are swallowed, so a stray letter can't act on
/// the shell behind the panel.
///
/// The help screen is the one exception: its list is longer than a 24-row pane
/// has lines, so `j`/`k` scroll it instead of doing nothing, and the pickers'
/// up/down step would be lost entirely without a way to reach the bottom rows.
fn map_overlay(ctrl: bool, alt: bool, key: KeyEvent, help: bool) -> Option<Action> {
    if help {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => return Some(Action::HelpScroll(1)),
            KeyCode::Char('k') | KeyCode::Up => return Some(Action::HelpScroll(-1)),
            KeyCode::PageUp => return Some(Action::HelpScroll(-5)),
            KeyCode::PageDown => return Some(Action::HelpScroll(5)),
            _ => {}
        }
    }
    match key.code {
        KeyCode::Char('c') if ctrl => Some(Action::Quit),
        KeyCode::Esc => Some(Action::CloseOverlay),
        KeyCode::Enter => Some(Action::OverlayConfirm),
        KeyCode::Up => Some(Action::OverlayStep(-1)),
        KeyCode::Down => Some(Action::OverlayStep(1)),
        KeyCode::Tab => Some(Action::OverlayStep(1)),
        KeyCode::BackTab => Some(Action::OverlayStep(-1)),
        KeyCode::Backspace => Some(Action::OverlayEdit(Edit::Backspace)),
        KeyCode::Delete => Some(Action::OverlayEdit(Edit::Delete)),
        KeyCode::Left => Some(Action::OverlayEdit(Edit::Left)),
        KeyCode::Right => Some(Action::OverlayEdit(Edit::Right)),
        KeyCode::Home => Some(Action::OverlayEdit(Edit::Home)),
        KeyCode::End => Some(Action::OverlayEdit(Edit::End)),
        KeyCode::Char('w') if ctrl => Some(Action::OverlayEdit(Edit::DeleteWordBack)),
        KeyCode::Char('u') if ctrl => Some(Action::OverlayEdit(Edit::DeleteToLineStart)),
        KeyCode::Char(ch) if !ctrl && !alt => Some(Action::OverlayEdit(Edit::Insert(ch))),
        _ => None,
    }
}

/// Keys the composer's `/` picker owns while it is open.
fn map_skill_menu(key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Up => Some(Action::SkillStep(-1)),
        KeyCode::Down => Some(Action::SkillStep(1)),
        KeyCode::Tab => Some(Action::SkillAccept),
        KeyCode::BackTab => Some(Action::SkillStep(-1)),
        KeyCode::Enter => Some(Action::SkillAccept),
        KeyCode::Esc => Some(Action::SkillDismiss),
        _ => None,
    }
}

fn map_composer(ctrl: bool, alt: bool, empty: bool, key: KeyEvent) -> Option<Action> {
    match key.code {
        // Enter sends; Alt-Enter and Ctrl-J insert a newline. Terminals without
        // the kitty protocol cannot report Shift-Enter at all, which is why the
        // newline binding is a modifier that always survives.
        KeyCode::Enter if alt || ctrl => Some(Action::Edit(Edit::Newline)),
        KeyCode::Enter => Some(Action::Send),
        KeyCode::Char('j') if ctrl => Some(Action::Edit(Edit::Newline)),
        KeyCode::Char('d') if ctrl && empty => Some(Action::Quit),
        KeyCode::Char('d') if ctrl => None,

        KeyCode::Esc => Some(Action::Focus(Focus::Transcript)),

        KeyCode::Char('w') if ctrl => Some(Action::Edit(Edit::DeleteWordBack)),
        KeyCode::Char('u') if ctrl => Some(Action::Edit(Edit::DeleteToLineStart)),
        KeyCode::Char('k') if ctrl => Some(Action::Edit(Edit::DeleteToLineEnd)),
        KeyCode::Char('a') if ctrl => Some(Action::Edit(Edit::Home)),
        KeyCode::Char('e') if ctrl => Some(Action::Edit(Edit::End)),
        KeyCode::Char('b') if alt => Some(Action::Edit(Edit::WordLeft)),
        KeyCode::Char('f') if alt => Some(Action::Edit(Edit::WordRight)),
        KeyCode::Backspace if alt || ctrl => Some(Action::Edit(Edit::DeleteWordBack)),

        KeyCode::Backspace => Some(Action::Edit(Edit::Backspace)),
        KeyCode::Delete => Some(Action::Edit(Edit::Delete)),
        KeyCode::Left if ctrl || alt => Some(Action::Edit(Edit::WordLeft)),
        KeyCode::Right if ctrl || alt => Some(Action::Edit(Edit::WordRight)),
        KeyCode::Left => Some(Action::Edit(Edit::Left)),
        KeyCode::Right => Some(Action::Edit(Edit::Right)),
        KeyCode::Up => Some(Action::Edit(Edit::Up)),
        KeyCode::Down => Some(Action::Edit(Edit::Down)),
        KeyCode::Home => Some(Action::Edit(Edit::Home)),
        KeyCode::End => Some(Action::Edit(Edit::End)),

        // Printable input. Control/alt combinations we didn't bind are dropped
        // rather than typed, so an unrecognized chord never injects a literal.
        KeyCode::Char(ch) if !ctrl && !alt => Some(Action::Edit(Edit::Insert(ch))),
        _ => None,
    }
}

fn map_sidebar(ctrl: bool, shift: bool, key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Char('q') => Some(Action::Quit),
        KeyCode::Char('?') => Some(Action::ToggleHelp),
        KeyCode::Char('r') if !ctrl => Some(Action::Reconnect),
        KeyCode::Char('n') => Some(Action::NewSession),
        KeyCode::Char('e') => Some(Action::ToggleArchive),
        KeyCode::Char('A') => Some(Action::ToggleShowArchived),
        KeyCode::Char('B') => Some(Action::ToggleBoard),
        KeyCode::Char('m') => Some(Action::ContextMenu),
        KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => Some(Action::Open),
        KeyCode::Char('i') => Some(Action::Focus(Focus::Composer)),
        KeyCode::Char('j') | KeyCode::Down => Some(Action::ListDown),
        KeyCode::Char('k') | KeyCode::Up => Some(Action::ListUp),
        KeyCode::Char('g') if !shift => Some(Action::ListTop),
        KeyCode::Char('G') => Some(Action::ListBottom),
        KeyCode::Home => Some(Action::ListTop),
        KeyCode::End => Some(Action::ListBottom),
        KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left => Some(Action::Focus(Focus::Transcript)),
        _ => None,
    }
}

fn map_transcript(ctrl: bool, shift: bool, key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Char('q') => Some(Action::Quit),
        KeyCode::Char('?') => Some(Action::ToggleHelp),
        KeyCode::Char('r') if !ctrl => Some(Action::Reconnect),
        KeyCode::Char('n') => Some(Action::NewSession),
        KeyCode::Char('A') => Some(Action::ToggleShowArchived),
        KeyCode::Char('B') => Some(Action::ToggleBoard),
        KeyCode::Enter | KeyCode::Char('i') => Some(Action::Focus(Focus::Composer)),
        KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left => Some(Action::Focus(Focus::Sidebar)),
        KeyCode::Char('j') | KeyCode::Down => Some(Action::ScrollDown(1)),
        KeyCode::Char('k') | KeyCode::Up => Some(Action::ScrollUp(1)),
        KeyCode::Char('d') if ctrl => Some(Action::PageDown),
        KeyCode::Char('u') if ctrl => Some(Action::PageUp),
        KeyCode::Char('g') if !shift => Some(Action::ScrollTop),
        KeyCode::Char('G') => Some(Action::ScrollBottom),
        KeyCode::Home => Some(Action::ScrollTop),
        KeyCode::End => Some(Action::ScrollBottom),
        _ => None,
    }
}

/// The help overlay's contents — one place, so the overlay can never drift from
/// [`map`].
pub const HELP: &[(&str, &str)] = &[
    ("Tab / Shift-Tab", "cycle panes"),
    ("Ctrl-B", "show/hide the sidebar"),
    ("j / k, ↓ / ↑", "move (list) or scroll (transcript)"),
    ("g / G", "jump to top / bottom"),
    ("Ctrl-U / Ctrl-D", "half page up / down"),
    ("PageUp / PageDown", "page the transcript from anywhere"),
    ("Enter", "open a session, or send the prompt"),
    ("i", "jump to the prompt"),
    ("Esc", "leave the prompt / step left"),
    ("Alt-Enter, Ctrl-J", "newline in the prompt"),
    (
        "Ctrl-W / Ctrl-U / Ctrl-K",
        "delete word / to line start / to line end",
    ),
    (
        "right-click, m",
        "context menu (rename, archive, pin as orchestrator, delete)",
    ),
    ("Alt-M", "switch model"),
    ("Alt-R", "branch for a new session"),
    ("Alt-W", "where a new session runs"),
    ("Alt-E", "reasoning effort"),
    ("Alt-← / Alt-→", "previous / next session tab"),
    ("Alt-1 … Alt-9", "jump to a session tab"),
    ("n", "new session in the selected space"),
    ("e", "archive or unarchive the selected session"),
    ("A", "show or hide archived sessions"),
    ("Ctrl-X", "interrupt the running agent"),
    ("r", "reconnect now"),
    ("B", "the task board (B again, esc, h: back)"),
    ("enter", "board: dispatch a ready task, retry a failed one"),
    ("space", "board: open the selected row (title, body, history, links)"),
    ("R", "board: retry — replaces a blocked task's live attempt"),
    ("f / F", "board: cycle the route filter / clear it"),
    ("/", "board: find as you type"),
    ("d", "board: which device hosts the board"),
    ("?", "this help"),
    ("q, Ctrl-C", "detach — the engine keeps running"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    fn alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    #[test]
    fn key_releases_are_ignored() {
        // Kitty-protocol terminals report Press and Release; acting on both
        // would type every character twice.
        let mut release = press(KeyCode::Char('a'));
        release.kind = KeyEventKind::Release;
        assert_eq!(map(Focus::Composer, false, true, release), None);
        let mut repeat = press(KeyCode::Char('a'));
        repeat.kind = KeyEventKind::Repeat;
        assert_eq!(
            map(Focus::Composer, false, true, repeat),
            Some(Action::Edit(Edit::Insert('a')))
        );
    }

    #[test]
    fn enter_sends_but_modified_enter_inserts_a_newline() {
        assert_eq!(
            map(Focus::Composer, false, false, press(KeyCode::Enter)),
            Some(Action::Send)
        );
        assert_eq!(
            map(Focus::Composer, false, false, alt(KeyCode::Enter)),
            Some(Action::Edit(Edit::Newline))
        );
        assert_eq!(
            map(Focus::Composer, false, false, ctrl('j')),
            Some(Action::Edit(Edit::Newline))
        );
    }

    #[test]
    fn letters_are_text_in_the_composer_and_commands_elsewhere() {
        // 'q' must never quit while typing a prompt.
        assert_eq!(
            map(Focus::Composer, false, false, press(KeyCode::Char('q'))),
            Some(Action::Edit(Edit::Insert('q')))
        );
        assert_eq!(
            map(Focus::Transcript, false, true, press(KeyCode::Char('q'))),
            Some(Action::Quit)
        );
        assert_eq!(
            map(Focus::Sidebar, false, true, press(KeyCode::Char('q'))),
            Some(Action::Quit)
        );
        // 'j' likewise.
        assert_eq!(
            map(Focus::Composer, false, false, press(KeyCode::Char('j'))),
            Some(Action::Edit(Edit::Insert('j')))
        );
        assert_eq!(
            map(Focus::Transcript, false, true, press(KeyCode::Char('j'))),
            Some(Action::ScrollDown(1))
        );
    }

    #[test]
    fn unbound_chords_never_type_a_literal() {
        // Ctrl-P isn't bound; it must be dropped, not inserted as 'p'.
        assert_eq!(map(Focus::Composer, false, false, ctrl('p')), None);
        assert_eq!(
            map(Focus::Composer, false, false, alt(KeyCode::Char('z'))),
            None
        );
    }

    #[test]
    fn ctrl_d_quits_only_on_an_empty_prompt() {
        assert_eq!(
            map(Focus::Composer, false, true, ctrl('d')),
            Some(Action::Quit)
        );
        assert_eq!(
            map(Focus::Composer, false, false, ctrl('d')),
            None,
            "Ctrl-D must not discard a half-written prompt"
        );
        // In the transcript it's a half page down instead.
        assert_eq!(
            map(Focus::Transcript, false, true, ctrl('d')),
            Some(Action::PageDown)
        );
    }

    #[test]
    fn global_bindings_work_from_every_pane() {
        for focus in [Focus::Sidebar, Focus::Transcript, Focus::Composer] {
            assert_eq!(map(focus, false, false, ctrl('c')), Some(Action::Quit));
            assert_eq!(map(focus, false, false, ctrl('x')), Some(Action::Interrupt));
            assert_eq!(
                map(focus, false, false, ctrl('b')),
                Some(Action::ToggleSidebar)
            );
            assert_eq!(
                map(focus, false, false, press(KeyCode::Tab)),
                Some(Action::FocusNext)
            );
            assert_eq!(
                map(focus, false, false, press(KeyCode::PageUp)),
                Some(Action::PageUp)
            );
        }
    }

    #[test]
    fn a_floating_panel_owns_the_keyboard() {
        // Esc dismisses, Enter activates, arrows move inside it.
        assert_eq!(
            map(Focus::Composer, true, false, press(KeyCode::Esc)),
            Some(Action::CloseOverlay)
        );
        assert_eq!(
            map(Focus::Composer, true, false, press(KeyCode::Enter)),
            Some(Action::OverlayConfirm)
        );
        assert_eq!(
            map(Focus::Composer, true, false, press(KeyCode::Down)),
            Some(Action::OverlayStep(1))
        );
        // Printable characters go to the panel's input, never to the shell
        // behind it — `q` must not quit while a rename field is open.
        assert_eq!(
            map(Focus::Transcript, true, true, press(KeyCode::Char('q'))),
            Some(Action::OverlayEdit(Edit::Insert('q')))
        );
        // Ctrl-C still gets you out of the app, not just the panel.
        assert_eq!(
            map(Focus::Composer, true, false, ctrl('c')),
            Some(Action::Quit)
        );
    }

    #[test]
    fn the_context_menu_and_model_picker_are_reachable() {
        // `m` outside the composer; Alt-M everywhere, including mid-prompt.
        assert_eq!(
            map(Focus::Sidebar, false, true, press(KeyCode::Char('m'))),
            Some(Action::ContextMenu)
        );
        assert_eq!(
            map(Focus::Composer, false, false, press(KeyCode::Char('m'))),
            Some(Action::Edit(Edit::Insert('m'))),
            "in the prompt it is just a letter"
        );
        for focus in [Focus::Sidebar, Focus::Transcript, Focus::Composer] {
            assert_eq!(
                map(focus, false, false, alt(KeyCode::Char('m'))),
                Some(Action::PickModel)
            );
        }
    }

    #[test]
    fn shifted_g_jumps_to_the_bottom() {
        // Terminals deliver Shift-g as an uppercase char, sometimes with the
        // SHIFT modifier set and sometimes without — both must work.
        let bare = KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE);
        let shifted = KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT);
        assert_eq!(
            map(Focus::Transcript, false, true, bare),
            Some(Action::ScrollBottom)
        );
        assert_eq!(
            map(Focus::Transcript, false, true, shifted),
            Some(Action::ScrollBottom)
        );
        assert_eq!(
            map(Focus::Transcript, false, true, press(KeyCode::Char('g'))),
            Some(Action::ScrollTop)
        );
    }

    #[test]
    fn focus_cycles_both_ways() {
        assert_eq!(Focus::Sidebar.next().next().next().next(), Focus::Sidebar);
        assert_eq!(Focus::Sidebar.previous(), Focus::Composer);
        assert_eq!(Focus::Transcript.previous(), Focus::Board);
        for focus in [Focus::Sidebar, Focus::Board, Focus::Transcript, Focus::Composer] {
            assert_eq!(focus.next().previous(), focus);
        }
    }

    #[test]
    fn escape_walks_left_out_of_the_prompt() {
        assert_eq!(
            map(Focus::Composer, false, false, press(KeyCode::Esc)),
            Some(Action::Focus(Focus::Transcript))
        );
        assert_eq!(
            map(Focus::Transcript, false, true, press(KeyCode::Esc)),
            Some(Action::Focus(Focus::Sidebar))
        );
    }

    #[test]
    fn the_board_owns_its_keys() {
        assert_eq!(
            map(Focus::Board, false, true, press(KeyCode::Char('j'))),
            Some(Action::BoardDown)
        );
        assert_eq!(
            map(Focus::Board, false, true, press(KeyCode::Char('k'))),
            Some(Action::BoardUp)
        );
        assert_eq!(
            map(Focus::Board, false, true, press(KeyCode::Enter)),
            Some(Action::BoardEnter)
        );
        assert_eq!(
            map(Focus::Board, false, true, press(KeyCode::Char('f'))),
            Some(Action::BoardCycleFilter)
        );
        assert_eq!(
            map(Focus::Board, false, true, press(KeyCode::Char('/'))),
            Some(Action::BoardFind)
        );
        assert_eq!(
            map(Focus::Board, false, true, press(KeyCode::Char('F'))),
            Some(Action::BoardClearFilter)
        );
        for closer in [
            press(KeyCode::Char('B')),
            press(KeyCode::Esc),
            press(KeyCode::Char('h')),
            press(KeyCode::Left),
        ] {
            assert_eq!(map(Focus::Board, false, true, closer), Some(Action::BoardClose));
        }
        // `q` still quits, `?` still reaches help.
        assert_eq!(
            map(Focus::Board, false, true, press(KeyCode::Char('q'))),
            Some(Action::Quit)
        );
        assert_eq!(
            map(Focus::Board, false, true, press(KeyCode::Char('?'))),
            Some(Action::ToggleHelp)
        );
    }

    #[test]
    fn the_board_find_field_takes_the_letters_while_it_is_open() {
        let typing = true;
        // Letters go to the query, not to the board.
        assert_eq!(
            map_with(Focus::Board, false, true, typing, false, false, false, press(KeyCode::Char('f'))),
            Some(Action::BoardFindType('f')),
            "with the field open, f must not filter"
        );
        assert_eq!(
            map_with(Focus::Board, false, true, typing, false, false, false, press(KeyCode::Char('q'))),
            Some(Action::BoardFindType('q')),
            "q must not quit while finding"
        );
        assert_eq!(
            map_with(Focus::Board, false, true, typing, false, false, false, press(KeyCode::Backspace)),
            Some(Action::BoardFindBackspace)
        );
        assert_eq!(
            map_with(Focus::Board, false, true, typing, false, false, false, press(KeyCode::Enter)),
            Some(Action::BoardFindAccept)
        );
        assert_eq!(
            map_with(Focus::Board, false, true, typing, false, false, false, press(KeyCode::Esc)),
            Some(Action::BoardFindEscape)
        );
        // Control chords are still dropped rather than typed.
        assert_eq!(map_with(Focus::Board, false, true, typing, false, false, false, ctrl('p')), None);
    }

    #[test]
    fn space_opens_a_row_and_enter_still_releases_it() {
        // From the list: space inspects, enter releases (gh#132).
        assert_eq!(
            map(Focus::Board, false, true, press(KeyCode::Char(' '))),
            Some(Action::BoardPeek)
        );
        assert_eq!(
            map(Focus::Board, false, true, press(KeyCode::Enter)),
            Some(Action::BoardEnter)
        );
    }

    #[test]
    fn an_open_row_owns_the_keyboard_but_never_the_dispatch_key() {
        let peek = true;
        let open = |code| map_with(Focus::Board, false, true, false, peek, false, false, press(code));
        assert_eq!(open(KeyCode::Char(' ')), Some(Action::BoardPeek));
        assert_eq!(open(KeyCode::Esc), Some(Action::BoardPeek));
        assert_eq!(open(KeyCode::Char('j')), Some(Action::BoardPeekScroll(1)));
        assert_eq!(open(KeyCode::Char('k')), Some(Action::BoardPeekScroll(-1)));
        // The detail is for reading and must not become a step on the way to a
        // release: enter still dispatches from inside it.
        assert_eq!(open(KeyCode::Enter), Some(Action::BoardEnter));
        // Everything else is swallowed — a stray `f` must not cycle a filter
        // on a list the panel is covering.
        assert_eq!(open(KeyCode::Char('f')), None);
        assert_eq!(open(KeyCode::Char('/')), None);
        assert_eq!(open(KeyCode::Char('R')), None);
    }

    #[test]
    fn the_skill_picker_takes_navigation_but_never_the_typing() {
        let menu = |key| map_with(Focus::Composer, false, true, false, false, false, true, key);
        assert_eq!(menu(press(KeyCode::Down)), Some(Action::SkillStep(1)));
        assert_eq!(menu(press(KeyCode::Up)), Some(Action::SkillStep(-1)));
        assert_eq!(menu(press(KeyCode::Enter)), Some(Action::SkillAccept));
        // Tab completes rather than switching pane — the picker outranks even
        // the bindings that mean the same thing everywhere else.
        assert_eq!(menu(press(KeyCode::Tab)), Some(Action::SkillAccept));
        assert_eq!(menu(press(KeyCode::Esc)), Some(Action::SkillDismiss));
        // Typing still filters: the picker must never swallow a keystroke.
        assert_eq!(
            menu(press(KeyCode::Char('c'))),
            Some(Action::Edit(Edit::Insert('c')))
        );
        assert_eq!(
            menu(press(KeyCode::Backspace)),
            Some(Action::Edit(Edit::Backspace))
        );

        // Closed, every one of those keys means what it always did.
        let plain = |key| map_with(Focus::Composer, false, true, false, false, false, false, key);
        assert_eq!(plain(press(KeyCode::Enter)), Some(Action::Send));
        assert_eq!(plain(press(KeyCode::Down)), Some(Action::Edit(Edit::Down)));
        assert_eq!(plain(press(KeyCode::Tab)), Some(Action::FocusNext));
        assert_eq!(
            plain(press(KeyCode::Esc)),
            Some(Action::Focus(Focus::Transcript))
        );

        // And the flag only reaches the composer: a picker cannot be open in
        // another pane, so it must not be able to steal that pane's keys.
        assert_eq!(
            map_with(
                Focus::Sidebar,
                false,
                true,
                false,
                false,
                false,
                true,
                press(KeyCode::Enter)
            ),
            Some(Action::Open)
        );
    }

    #[test]
    fn b_toggles_the_board_from_the_chat_panes() {
        for focus in [Focus::Sidebar, Focus::Transcript] {
            assert_eq!(
                map(focus, false, true, press(KeyCode::Char('B'))),
                Some(Action::ToggleBoard),
                "{focus:?} must open the board"
            );
        }
        // Not in the composer, where capital letters are input.
        assert_eq!(
            map(Focus::Composer, false, false, press(KeyCode::Char('B'))),
            Some(Action::Edit(Edit::Insert('B')))
        );
    }

    #[test]
    fn board_focus_cycles_through_the_tab_order() {
        assert_eq!(Focus::Sidebar.next(), Focus::Board);
        assert_eq!(Focus::Board.next(), Focus::Transcript);
        assert_eq!(Focus::Composer.previous(), Focus::Transcript);
        assert_eq!(Focus::Board.previous(), Focus::Sidebar);
        for focus in [
            Focus::Sidebar,
            Focus::Board,
            Focus::Transcript,
            Focus::Composer,
        ] {
            assert_eq!(focus.next().previous(), focus);
        }
    }
}
