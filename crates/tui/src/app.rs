//! Application state and the reducer over [`Action`] / [`Update`].
//!
//! Everything here is synchronous and free of I/O: an action folds into state
//! and returns [`Effects`] — the RPC calls the app wants made — which the event
//! loop hands to the engine link. That keeps the whole interaction model
//! testable without a terminal or a daemon, and it keeps the draw loop's only
//! async dependency the two channels in `main`.
//!
//! One structural choice worth naming: the sidebar is kept as a **flattened row
//! list** rebuilt only when the data behind it changes, never per frame. Cursor
//! movement is then an index step, selection is an index read, and rendering is
//! a slice — none of which have to re-derive groups, sorts, or indicators. Those
//! derivations are the expensive part, and they only actually change when a
//! watch frame arrives.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use comet_doc::{MessagePart, MessageRole, SessionCommandPayload, SessionMessageEntry};
use comet_proto::view::{
    self, CheckoutKind, CheckoutPlan, ConnectionStatus, GatePhase, Indicator, display_status,
    format_time_ago,
};
use comet_proto::view::board::{self as board_view, AgentState, BoardState};
use comet_proto::{
    AuthState, Chat, ChatIndicator, Device, RunRequest, SandboxLevel, Session, Space,
};
use comet_rpc::methods;

use crate::board::Board;
use crate::composer::Composer;
use crate::daemon::Attachment;
use crate::keys::{Action, Edit, Focus};
use crate::link::{BoardHost, Command, Update};
use crate::transcript::Transcript;

/// RPC work an action produced. Returned rather than performed so the reducer
/// stays pure.
pub type Effects = Vec<Command>;

/// Human label for an effort level.
pub fn reasoning_label(level: comet_proto::ReasoningLevel) -> &'static str {
    use comet_proto::ReasoningLevel as R;
    match level {
        R::Minimal => "Minimal",
        R::Low => "Low",
        R::Medium => "Medium",
        R::High => "High",
        R::XHigh => "XHigh",
        R::Max => "Max",
        R::Ultra => "Ultra",
        R::Ultracode => "Ultracode",
        R::Ultrathink => "Ultrathink",
    }
}

/// Apply a text edit to a buffer. Shared by the composer and the overlay
/// prompt so a rename field behaves exactly like the prompt does.
fn apply_edit(buffer: &mut Composer, edit: Edit) {
    match edit {
        Edit::Insert(ch) => buffer.insert_char(ch),
        Edit::Paste(text) => buffer.insert_str(&text),
        Edit::Newline => buffer.insert_newline(),
        Edit::Backspace => buffer.backspace(),
        Edit::Delete => buffer.delete(),
        Edit::DeleteWordBack => buffer.delete_word_back(),
        Edit::DeleteToLineStart => buffer.delete_to_line_start(),
        Edit::DeleteToLineEnd => buffer.delete_to_line_end(),
        Edit::Left => buffer.left(),
        Edit::Right => buffer.right(),
        Edit::WordLeft => buffer.word_left(),
        Edit::WordRight => buffer.word_right(),
        Edit::Home => buffer.home(),
        Edit::End => buffer.end(),
        Edit::BufferStart => buffer.to_start(),
        Edit::BufferEnd => buffer.to_end(),
        // Vertical movement is the composer's own concern (it falls through to
        // the transcript at the edges); a one-line prompt has nowhere to go.
        Edit::Up | Edit::Down => {}
    }
}

/// How long a status-line notice stays up before the key hints come back.
const NOTICE_TTL: std::time::Duration = std::time::Duration::from_secs(6);

/// How long a just-created chat's selection is protected from being healed away
/// while its row is still in flight. Long enough for a slow relay-forwarded
/// mutation, short enough that a `createChat` which genuinely failed doesn't
/// leave the viewport pointed at a session that will never exist.
const PENDING_CHAT_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// One row of the flattened sidebar.
///
/// This is comet-native's information architecture, not the original Electron
/// app's: **two sections**, not one grouped list. The desktop shell
/// (`comet-ui/src/shell.rs::render_chat_sidebar`) reads
///
/// - **Spaces** — the (device, folder) pairs, each one line of "name · device",
///   carrying an aggregate attention dot so a live session is visible even when
///   the Sessions list is scrolled away;
/// - **Sessions** — a *flat, global, attention-sorted* list of every visible
///   session across every space. Deliberately not grouped: the point of the
///   list is "what needs me right now", which grouping would bury.
///
/// The selected space's sessions appear separately, as the tab strip above the
/// transcript ([`App::tabs`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    /// A section header: label left, optional affordance right.
    Section {
        label: String,
        action: Option<String>,
    },
    /// A space: folder name plus the device hosting it.
    Space {
        id: String,
        label: String,
        device: String,
        /// Most urgent status among this space's sessions, if any is live.
        attention: Option<ChatIndicator>,
        /// The host's presence heartbeat has lapsed.
        offline: bool,
    },
    /// A live board attempt: state glyph + issue identifier + elapsed, then an
    /// indented branch. Above the sessions, because an agent that is stuck is
    /// the one thing on this pane worth interrupting a person for (gh#103).
    Agent {
        /// The chat the row opens — namespaced in [`Row::key`] so the cursor
        /// can tell this row from the same chat's entry in the sessions list.
        chat_id: String,
        identifier: String,
        branch: Option<String>,
        state: AgentState,
        /// When the attempt started, and what it is capped at — the instant, not
        /// the age, so the counter moves on a redraw instead of needing the
        /// whole sidebar rebuilt once a second.
        started_at: Option<DateTime<Utc>>,
        cap_secs: Option<u64>,
    },
    /// A working chat with no attempt behind it (gh#117): state glyph + the
    /// chat's own title + elapsed. One line — there is no branch promised and
    /// no issue to name, so a second line would be an empty one.
    Running {
        /// Namespaced in [`Row::key`] like [`Row::Agent`], for the same reason:
        /// the same chat also has a row in the Sessions list.
        chat_id: String,
        space_id: Option<String>,
        title: String,
        /// `Working` or `Blocked` only — membership is the live indicator.
        state: AgentState,
        /// When the RUN started, off the session mirror. The instant, not the
        /// age, so the counter moves on a redraw.
        started_at: Option<DateTime<Utc>>,
    },
    /// A session: dot + title + time, then an indented "space@device".
    Chat {
        id: String,
        space_id: Option<String>,
        title: String,
        location: Option<String>,
        indicator: ChatIndicator,
        archived: bool,
        activity: Option<DateTime<Utc>>,
        /// The board's pinned orchestrator (gh#104) — first in the list and
        /// marked, because it is the one session that is about the board rather
        /// than about a task, and losing it in a recency-sorted list is how it
        /// stops being the thing you talk to.
        orchestrator: bool,
    },
    /// Placeholder text for an empty section.
    Empty { label: String },
    /// Vertical air between sections (the desktop sidebar's 12px lead-in).
    Blank,
    /// The signed-in user, pinned at the bottom.
    User { name: String, email: String },
}

impl Row {
    /// What this row *addresses* — a space, or a chat in the Sessions list.
    /// Decoration addresses nothing, and neither does an agent or running row:
    /// each points at a chat the Sessions list already answers for, and two
    /// rows answering to one id would let "put the cursor on chat X" land on
    /// either. Cursor identity is [`Row::key`].
    pub fn id(&self) -> Option<&str> {
        match self {
            Row::Space { id, .. } | Row::Chat { id, .. } => Some(id),
            _ => None,
        }
    }

    /// Which row this *is*, for the identity-preserving cursor across rebuilds.
    ///
    /// An agent or running row shares its chat with the Sessions list, so its
    /// key is namespaced with the NUL prefix the board pane's section headers
    /// use — otherwise a rebuild would slide the cursor between the lists.
    pub fn key(&self) -> Option<String> {
        match self {
            Row::Agent { chat_id, .. } => Some(format!("\u{0}agent:{chat_id}")),
            Row::Running { chat_id, .. } => Some(format!("\u{0}running:{chat_id}")),
            other => other.id().map(str::to_string),
        }
    }

    /// Lines this row occupies.
    pub fn height(&self) -> u16 {
        match self {
            // Title and sub-line, with no row of air between entries: at one
            // blank row per session the list reads as a set of loose cards
            // rather than a list, and a terminal has no half-row to spend
            // instead. The breathing room comes from the pane's own padding.
            Row::Chat { .. } => 2,
            // Identifier over branch — the same two-line shape, so the two
            // lists read as one pane rather than two designs.
            Row::Agent { .. } => 2,
            // One line: the sub-line under an agent row carries its branch, and
            // an unmanaged run has none to carry.
            Row::Running { .. } => 1,
            // An account with no display name is one line, not one line and a
            // blank: the second row exists to carry the email *under* a name.
            Row::User { name, email } => {
                if name.is_empty() || email.is_empty() || name == email {
                    1
                } else {
                    2
                }
            }
            _ => 1,
        }
    }

    /// Can the cursor land here? Decoration cannot.
    pub fn selectable(&self) -> bool {
        matches!(
            self,
            Row::Space { .. } | Row::Chat { .. } | Row::Agent { .. } | Row::Running { .. }
        )
    }
}

/// Something the pointer can land on.
///
/// A cell grid has no widget tree, so clicks are resolved against a hit map the
/// renderer rebuilds each frame: draw a thing, record where you drew it. Regions
/// are tested newest-first, so an overlay drawn last wins over the pane beneath
/// it — the equivalent of z-order.
#[derive(Debug, Clone, PartialEq)]
pub enum Hit {
    /// A sidebar row, by index into [`App::rows`].
    Row(usize),
    /// A session tab, by index into [`App::tabs`].
    Tab(usize),
    /// The `+` at the end of the tab strip.
    NewSession,
    /// The `+` on the Spaces header.
    AddSpace,
    /// A board row, by index into the lines the last render drew.
    BoardRow(usize),
    /// One of the composer's chips.
    Chip(ChipKind),
    /// A pane, which takes focus.
    Pane(Focus),
    /// The help overlay — a click anywhere on it dismisses.
    Overlay,
}

/// A session being composed but not yet created.
///
/// The desktop app does not mint a chat when you click `+` — it opens a canvas,
/// you choose where the work should run, and the session materializes on the
/// first send (`shell/tabs.rs`: "the tab materializes on first send"). Doing the
/// same here means an abandoned draft leaves nothing behind, and the checkout
/// choice is made *before* anything exists to be wrong about.
#[derive(Debug, Clone)]
pub struct Draft {
    pub space_id: String,
    pub checkout: CheckoutKind,
    /// Index into `refs` of the picked base ref.
    pub picked_ref: Option<usize>,
    /// Branches for the space, once `ListRefs` answers. `None` while loading;
    /// empty for a space that is not a git checkout.
    pub refs: Option<Vec<comet_proto::RepoRef>>,
    /// Model and effort chosen before the session exists. Carried onto
    /// `createChat` so the first run starts on the right settings.
    pub model: Option<comet_proto::Model>,
    pub reasoning: Option<comet_proto::ReasoningLevel>,
}

impl Draft {
    pub fn selected_ref(&self) -> Option<&comet_proto::RepoRef> {
        let refs = self.refs.as_ref()?;
        match self.picked_ref {
            Some(index) => refs.get(index),
            // Default to the checkout's current branch, as the desktop picker
            // starts there.
            None => refs.iter().find(|candidate| candidate.current),
        }
    }

    pub fn plan(&self) -> CheckoutPlan {
        view::checkout_plan(self.checkout, self.selected_ref())
    }

    pub fn checkout_label(&self) -> &'static str {
        view::checkout_label(self.checkout, self.selected_ref())
    }

    pub fn ref_label(&self) -> String {
        self.selected_ref()
            .map(|candidate| candidate.name.clone())
            .unwrap_or_else(|| "Select ref".to_string())
    }
}

/// One tab in the strip above the transcript: a session of the selected space.
#[derive(Debug, Clone, PartialEq)]
pub struct Tab {
    pub id: String,
    pub title: String,
    pub indicator: ChatIndicator,
    pub active: bool,
}

/// A floating panel over the shell: a right-click menu, the model picker, or a
/// rename prompt. Only one is up at a time, as in the desktop app.
pub enum Overlay {
    /// Right-click menu, anchored at the pointer.
    Menu {
        title: String,
        items: Vec<MenuItem>,
        active: usize,
        column: u16,
        row: u16,
    },
    /// Branch picker for a draft.
    Refs { active: usize },
    /// Where a drafted session should run.
    Checkout { active: usize },
    /// Effort picker, from the active model's levels.
    Reasoning {
        levels: Vec<comet_proto::ReasoningLevel>,
        active: usize,
    },
    /// Model picker for the open session's harness.
    Models {
        /// `None` until `ListModels` answers.
        models: Option<Vec<comet_proto::Model>>,
        active: usize,
    },
    /// Whose agent account a board dispatch should spend (gh#74). Opened by
    /// `enter` on a ready row, ahead of the release itself: row 0 is the
    /// route's own account (no override — what enter always did), and the rest
    /// are the host's saved logins for that row's harness.
    DispatchAccount {
        task_id: String,
        identifier: String,
        /// `None` until `ListAgentAccounts` answers; already filtered to the
        /// logins the row's runtime can spend.
        accounts: Option<Vec<comet_proto::AgentAccount>>,
        /// What row 0 will spend when nothing is picked — the route's account,
        /// when the row names one.
        route_account: Option<String>,
        /// The harness the row's runtime resolves to, as the host's
        /// `ListBoardRuntimes` reported it. `None` when that lookup failed, and
        /// then the picker says nothing about who pays: an unfiltered list has
        /// no reliable "the box's own login" in it, and a wrong guess about
        /// whose subscription a row spends is worse than no guess (gh#101).
        harness: Option<comet_proto::HarnessId>,
        active: usize,
    },
    /// A single-line text prompt (rename).
    Prompt {
        title: String,
        input: Composer,
        action: PromptAction,
    },
}

/// The composer's buttons. Each is separately clickable, because they open
/// different pickers — one lumped target sends every click to the same place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipKind {
    Branch,
    Checkout,
    Model,
    Reasoning,
}

/// Which model answers, and at what effort. See [`App::composer_meta`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerMeta {
    /// Never empty — a placeholder stands in while the real label loads.
    pub model: String,
    /// Absent when the model exposes no effort levels, or none is chosen.
    pub effort: Option<String>,
}

/// The ref this work sits on and where it runs. See [`App::composer_footer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerFooter {
    pub reference: String,
    pub checkout: String,
    /// False for an open session, whose checkout kind was fixed at creation.
    pub checkout_live: bool,
}

/// One row of a context menu.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuItem {
    pub label: String,
    pub action: MenuAction,
    /// Draw a separator above this item.
    pub separated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MenuAction {
    RenameChat(String),
    SetArchived(String, bool),
    DeleteChat(String),
    /// Pin this chat as the board's orchestrator, or (`None`) unpin whatever is
    /// pinned. One per board, so pinning is a move rather than an add.
    SetOrchestrator(Option<String>),
    RenameSpace(String),
    DeleteSpace(String),
    PickModel,
}

/// What a [`Overlay::Prompt`] does with its text on Enter.
#[derive(Debug, Clone, PartialEq)]
pub enum PromptAction {
    RenameChat(String),
    RenameSpace(String),
}

pub struct Notice {
    pub text: String,
    /// When it stops being shown. A monotonic instant, not a wall clock: the
    /// event loop sleeps until exactly this moment, and a clock adjustment must
    /// not strand a notice on screen forever.
    pub until: std::time::Instant,
}

pub struct App {
    pub focus: Focus,
    pub connection: ConnectionStatus,
    pub attachment: Option<Attachment>,
    pub auth: Option<AuthState>,

    pub devices: Vec<Device>,
    pub spaces: Vec<Space>,
    /// Sorted by `comet_proto::view::sort_chats`; includes archived rows.
    pub chats: Vec<Chat>,
    pub sessions: Vec<Session>,
    pub local_device_id: Option<String>,

    pub selected_space: Option<String>,
    pub selected_chat: Option<String>,
    /// A session being composed. Mutually exclusive with `selected_chat`.
    pub draft: Option<Draft>,
    /// Last session opened in each space. Switching spaces returns you where
    /// you were, as the desktop app does — a space is a place you come back to,
    /// not a filter you re-navigate every time.
    last_visited: HashMap<String, String>,

    /// Flattened sidebar rows. Rebuilt on data change, not per frame.
    pub rows: Vec<Row>,
    pub cursor: usize,
    pub show_archived: bool,
    pub sidebar_visible: bool,

    pub transcript: Transcript,
    pub composer: Composer,
    pub help: bool,
    /// Where the help overlay is scrolled to. Its list is longer than a pane,
    /// and a reference that silently stops halfway is how keys stay unknown.
    pub help_scroll: usize,
    /// The board pane: the derivations come from `comet_proto::view::board`,
    /// the interaction state lives here. `board_open` decides whether the main
    /// pane is the board at all.
    pub board: Board,
    pub board_open: bool,
    /// Which device's board is on screen (gh#55): `None` = this device. Owned
    /// by the supervisor — the board RPCs are relay-forwardable, so a laptop
    /// hosting no board still reads and drives the box's.
    pub board_host: Option<String>,
    /// The chat that board has pinned as its orchestrator (gh#104), as its
    /// `WatchBoardOrchestrator` stream reports it. `None` = no pin, or a board
    /// this pane has not found yet.
    pub orchestrator: Option<String>,
    /// That device has actually delivered rows. Until it does, the header says
    /// who it is asking rather than claiming an answer.
    pub board_host_live: bool,
    /// What the operator chose with `d`; the supervisor sweeps when this is
    /// [`BoardHost::Auto`].
    pub board_pin: BoardHost,
    /// The one floating panel, if any.
    pub overlay: Option<Overlay>,
    pub notice: Option<Notice>,
    /// When the viewport started. Loaders are pure functions of the elapsed
    /// time since then, so animation is frame-rate independent and a dropped
    /// frame doesn't shift the wave.
    started: std::time::Instant,
    pub quit: bool,

    /// A chat this viewport just created, with when it asked. Exempt from
    /// selection healing until its row arrives — see `heal_chat_selection`.
    pending_chat: Option<(String, std::time::Instant)>,
    /// Optimistic user messages per chat, shown until the doc frame with the
    /// same id arrives. Client-minted ids make the dedupe exact.
    echoes: HashMap<String, Vec<SessionMessageEntry>>,
    /// The selected chat's latest doc snapshot. Held (rather than pushed
    /// straight into the transcript) so a burst of watch frames between two
    /// draws costs one layout pass instead of one per frame.
    doc_entries: Vec<SessionMessageEntry>,
    /// Width the transcript was last laid out at.
    transcript_width: u16,
    /// Set when entries/echoes changed and the transcript needs re-laying out.
    transcript_stale: bool,
    /// Rows of the transcript pane, as of the last draw. Scroll actions need it
    /// and only the renderer knows the real geometry.
    viewport_height: usize,
    /// Palette. Held here because layout bakes styles into cached lines.
    pub theme: crate::theme::Theme,
    /// Where things were drawn last frame, for click resolution. Rebuilt by the
    /// renderer every draw; never read except on a mouse event.
    hits: Vec<(ratatui::layout::Rect, Hit)>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self::with_theme(crate::theme::Theme::from_env())
    }

    pub fn with_theme(theme: crate::theme::Theme) -> Self {
        let mut app = Self {
            focus: Focus::Composer,
            connection: ConnectionStatus::Connecting,
            attachment: None,
            auth: None,
            devices: Vec::new(),
            spaces: Vec::new(),
            chats: Vec::new(),
            sessions: Vec::new(),
            local_device_id: None,
            selected_space: None,
            selected_chat: None,
            draft: None,
            last_visited: HashMap::new(),
            rows: Vec::new(),
            cursor: 0,
            show_archived: false,
            sidebar_visible: true,
            transcript: Transcript::new(),
            composer: Composer::default(),
            help: false,
            help_scroll: 0,
            board: Board::new(),
            board_open: false,
            board_host: None,
            orchestrator: None,
            board_host_live: false,
            board_pin: BoardHost::Auto,
            overlay: None,
            notice: None,
            started: std::time::Instant::now(),
            quit: false,
            pending_chat: None,
            echoes: HashMap::new(),
            doc_entries: Vec::new(),
            transcript_width: 0,
            transcript_stale: false,
            viewport_height: 1,
            theme,
            hits: Vec::new(),
        };
        // The sidebar chrome exists before any data arrives: an empty workspace
        // still needs a reachable "New session".
        app.rebuild_rows();
        app
    }

    // -----------------------------------------------------------------------
    // Engine updates
    // -----------------------------------------------------------------------

    pub fn apply(&mut self, update: Update) -> Effects {
        match update {
            Update::Connection(status) => {
                self.connection = status;
                // The device row's presence dot follows the connection.
                self.rebuild_rows();
                Vec::new()
            }
            Update::Attached(attachment) => {
                self.attachment = Some(attachment);
                Vec::new()
            }
            Update::Auth(state) => {
                self.auth = Some(*state);
                // The user row appears once the engine reports a session.
                self.rebuild_rows();
                Vec::new()
            }
            Update::Devices(devices) => {
                self.devices = devices;
                // The device row names this engine once the registry arrives.
                self.rebuild_rows();
                Vec::new()
            }
            Update::Spaces(mut spaces) => {
                view::sort_spaces(&mut spaces);
                self.spaces = spaces;
                self.heal_space_selection();
                self.rebuild_rows();
                Vec::new()
            }
            Update::Chats(mut chats) => {
                view::sort_chats(&mut chats);
                self.chats = chats;
                // Rows first: healing the selection wants to move the cursor
                // onto the row it picked, which has to exist by then.
                self.rebuild_rows();
                self.heal_chat_selection()
            }
            Update::Sessions(sessions) => {
                self.sessions = sessions;
                // Status dots live on the rows, so a session frame has to
                // refresh them — but nothing about the row *set* changed, so
                // selection and cursor are preserved by rebuild_rows.
                self.rebuild_rows();
                Vec::new()
            }
            Update::Transcript { chat_id, entries } => {
                // A frame for a chat we've since navigated away from is stale.
                if self.selected_chat.as_deref() == Some(chat_id.as_str()) {
                    if let Some(echoes) = self.echoes.get_mut(&chat_id) {
                        echoes.retain(|echo| !entries.iter().any(|e| e.id == echo.id));
                        if echoes.is_empty() {
                            self.echoes.remove(&chat_id);
                        }
                    }
                    self.doc_entries = entries;
                    self.transcript_stale = true;
                }
                Vec::new()
            }
            Update::LocalDevice(device_id) => {
                self.local_device_id = Some(device_id);
                self.rebuild_rows();
                Vec::new()
            }
            Update::Refs(refs) => {
                if let Some(draft) = &mut self.draft {
                    draft.refs = Some(refs);
                }
                Vec::new()
            }
            Update::SessionStarted { chat_id } => {
                // The draft became real: adopt it as the open session. The chats
                // frame that materializes the row follows.
                self.draft = None;
                self.pending_chat = Some((chat_id.clone(), std::time::Instant::now()));
                let effects = self.select_chat(Some(chat_id));
                self.rebuild_rows();
                effects
            }
            Update::Models(models) => {
                // Only fills a picker that is still open — a slow reply must not
                // pop one back up after it was dismissed.
                if let Some(Overlay::Models { models: slot, .. }) = &mut self.overlay {
                    *slot = Some(models);
                }
                Vec::new()
            }
            Update::DispatchAccounts {
                task_id: for_task,
                accounts,
                harness: for_harness,
            } => {
                // Only fills the picker it was asked for: a slow reply must not
                // land the wrong row's logins under a newer pick.
                if let Some(Overlay::DispatchAccount {
                    task_id,
                    accounts: slot,
                    harness,
                    ..
                }) = &mut self.overlay
                    && *task_id == for_task
                {
                    *slot = Some(accounts);
                    *harness = for_harness;
                }
                Vec::new()
            }
            Update::Notice(text) => {
                // A notice while a picker is waiting means the fetch failed;
                // close it rather than leaving a spinner forever.
                if matches!(self.overlay, Some(Overlay::Models { models: None, .. }))
                    || matches!(
                        self.overlay,
                        Some(Overlay::DispatchAccount { accounts: None, .. })
                    )
                {
                    self.overlay = None;
                }
                self.notify(text);
                Vec::new()
            }
            Update::Board(rows) => {
                self.board.set_rows(rows);
                // The sidebar's Agents section is built from these, so a board
                // frame is a sidebar change even with the board pane closed.
                self.rebuild_rows();
                Vec::new()
            }
            Update::BoardHostChanged { device, live } => {
                self.board_host = device;
                self.board_host_live = live;
                Vec::new()
            }
            Update::Orchestrator(chat_id) => {
                self.orchestrator = chat_id;
                // The pin decides a row's *position*, so the list has to be
                // rebuilt rather than merely repainted.
                self.rebuild_rows();
                Vec::new()
            }
            Update::SendFailed {
                chat_id,
                message_id,
                error,
            } => {
                // Hand the text back rather than losing it: the echo carries the
                // only copy once the composer was cleared.
                let restored = self.drop_echo(&chat_id, &message_id);
                if let Some(text) = restored
                    && self.composer.is_empty()
                {
                    self.composer.set_text(text);
                }
                self.transcript_stale = true;
                self.notify(format!("Couldn't send: {error}"));
                Vec::new()
            }
        }
    }

    // -----------------------------------------------------------------------
    // Actions
    // -----------------------------------------------------------------------

    pub fn act(&mut self, action: Action) -> Effects {
        match action {
            Action::Quit => {
                self.quit = true;
                Vec::new()
            }
            Action::ToggleHelp => {
                self.help = !self.help;
                self.help_scroll = 0;
                Vec::new()
            }
            Action::HelpScroll(delta) => {
                self.help_scroll = self.help_scroll.saturating_add_signed(delta);
                Vec::new()
            }
            Action::CloseOverlay => {
                if self.overlay_open() {
                    self.close_overlay();
                    return Vec::new();
                }
                // Nothing floating: Esc abandons a draft, which leaves no trace
                // because nothing was created.
                if self.draft.take().is_some() {
                    self.rebuild_rows();
                    return self.heal_chat_selection();
                }
                Vec::new()
            }
            Action::ContextMenu => {
                // Keyboard parity with right-click: anchor beside the sidebar.
                self.open_context_menu(2, self.cursor.min(u16::MAX as usize) as u16);
                Vec::new()
            }
            Action::PickModel => self.open_model_picker(),
            Action::PickRef => self.open_ref_picker(),
            Action::PickCheckout => self.open_checkout_picker(),
            Action::PickReasoning => self.open_reasoning_picker(),
            Action::OverlayStep(delta) => {
                self.overlay_step(delta);
                Vec::new()
            }
            Action::OverlayConfirm => self.overlay_confirm(),
            Action::OverlayEdit(edit) => {
                if let Some(Overlay::Prompt { input, .. }) = &mut self.overlay {
                    apply_edit(input, edit);
                }
                Vec::new()
            }
            Action::Focus(focus) => {
                self.focus = focus;
                Vec::new()
            }
            Action::FocusNext => {
                self.focus = self.skip_hidden(self.focus.next(), true);
                Vec::new()
            }
            Action::FocusPrevious => {
                self.focus = self.skip_hidden(self.focus.previous(), false);
                Vec::new()
            }
            Action::ToggleSidebar => {
                self.sidebar_visible = !self.sidebar_visible;
                if !self.sidebar_visible && self.focus == Focus::Sidebar {
                    self.focus = Focus::Transcript;
                }
                Vec::new()
            }
            Action::Reconnect => vec![Command::Reconnect],

            // Blanks, section headers and the user row are decoration: the
            // cursor steps over them rather than stopping on nothing.
            Action::ListUp => {
                if let Some(at) = self.next_selectable(self.cursor, -1) {
                    self.cursor = at;
                }
                Vec::new()
            }
            Action::ListDown => {
                if let Some(at) = self.next_selectable(self.cursor, 1) {
                    self.cursor = at;
                }
                Vec::new()
            }
            Action::ListTop => {
                self.cursor = self.first_selectable();
                Vec::new()
            }
            Action::ListBottom => {
                self.cursor = self.rows.iter().rposition(Row::selectable).unwrap_or(0);
                Vec::new()
            }
            Action::Open => self.open_row(),
            Action::NewSession => self.new_session(),
            Action::ToggleArchive => self.toggle_archive(),
            Action::ToggleShowArchived => {
                self.show_archived = !self.show_archived;
                self.rebuild_rows();
                Vec::new()
            }

            Action::CycleTab(step) => self.cycle_tab(step),
            Action::SelectTab(index) => self.select_tab(index),

            Action::ScrollUp(lines) => {
                self.transcript
                    .scroll_up(lines as usize, self.viewport_height);
                Vec::new()
            }
            Action::ScrollDown(lines) => {
                self.transcript
                    .scroll_down(lines as usize, self.viewport_height);
                Vec::new()
            }
            Action::PageUp => {
                let page = (self.viewport_height / 2).max(1);
                self.transcript.scroll_up(page, self.viewport_height);
                Vec::new()
            }
            Action::PageDown => {
                let page = (self.viewport_height / 2).max(1);
                self.transcript.scroll_down(page, self.viewport_height);
                Vec::new()
            }
            Action::ScrollTop => {
                self.transcript.to_top();
                Vec::new()
            }
            Action::ScrollBottom => {
                self.transcript.to_bottom();
                Vec::new()
            }

            Action::Send => self.send(),
            Action::Interrupt => self.interrupt(),
            Action::Edit(edit) => {
                self.edit(edit);
                Vec::new()
            }

            // Board
            Action::ToggleBoard => {
                self.toggle_board();
                Vec::new()
            }
            Action::BoardClose => {
                self.close_board();
                Vec::new()
            }
            Action::BoardUp => {
                self.board.select_delta(-1);
                Vec::new()
            }
            Action::BoardDown => {
                self.board.select_delta(1);
                Vec::new()
            }
            Action::BoardPage(delta) => {
                self.board.select_page(delta);
                Vec::new()
            }
            Action::BoardTop => {
                self.board.select_top();
                Vec::new()
            }
            Action::BoardBottom => {
                self.board.select_bottom();
                Vec::new()
            }
            Action::BoardEnter => self.board_enter(),
            Action::BoardRetry => self.board_retry(),
            Action::BoardCycleFilter => {
                if let Some(message) = self.board.cycle_filter() {
                    self.notify(message);
                }
                Vec::new()
            }
            Action::BoardFind => {
                self.board.open_find();
                Vec::new()
            }
            Action::BoardClearFilter => {
                self.board.clear_filter();
                Vec::new()
            }
            Action::BoardCycleHost => self.cycle_board_host(),
            Action::BoardFindType(ch) => {
                self.board.find_type(ch);
                Vec::new()
            }
            Action::BoardFindBackspace => {
                self.board.find_backspace();
                Vec::new()
            }
            Action::BoardFindAccept => {
                self.board.accept_find();
                Vec::new()
            }
            Action::BoardFindEscape => {
                self.board.escape_find();
                Vec::new()
            }
        }
    }

    fn edit(&mut self, edit: Edit) {
        match edit {
            Edit::Insert(ch) => self.composer.insert_char(ch),
            Edit::Paste(text) => self.composer.insert_str(&text),
            Edit::Newline => self.composer.insert_newline(),
            Edit::Backspace => self.composer.backspace(),
            Edit::Delete => self.composer.delete(),
            Edit::DeleteWordBack => self.composer.delete_word_back(),
            Edit::DeleteToLineStart => self.composer.delete_to_line_start(),
            Edit::DeleteToLineEnd => self.composer.delete_to_line_end(),
            Edit::Left => self.composer.left(),
            Edit::Right => self.composer.right(),
            Edit::WordLeft => self.composer.word_left(),
            Edit::WordRight => self.composer.word_right(),
            // At the buffer's edges, vertical movement leaves the composer
            // instead of doing nothing — the caret has somewhere to go.
            Edit::Up => {
                if !self.composer.up() {
                    self.transcript.scroll_up(1, self.viewport_height);
                }
            }
            Edit::Down => {
                if !self.composer.down() {
                    self.transcript.scroll_down(1, self.viewport_height);
                }
            }
            Edit::Home => self.composer.home(),
            Edit::End => self.composer.end(),
            Edit::BufferStart => self.composer.to_start(),
            Edit::BufferEnd => self.composer.to_end(),
        }
    }

    /// Tab past a pane that isn't on screen.
    fn skip_hidden(&self, focus: Focus, forward: bool) -> Focus {
        let mut at = focus;
        // Loop, not a single skip: with the board open, Transcript and Composer
        // are both hidden, so one step could land on the second of them.
        for _ in 0..4 {
            if self.pane_visible(at) {
                return at;
            }
            at = if forward { at.next() } else { at.previous() };
        }
        at
    }

    /// Whether a pane is on screen right now. The board replaces the main pane
    /// when it is open, so Transcript and Composer only exist when it is not.
    fn pane_visible(&self, focus: Focus) -> bool {
        match focus {
            Focus::Sidebar => self.sidebar_visible,
            Focus::Board => self.board_open,
            Focus::Transcript | Focus::Composer => !self.board_open,
        }
    }

    // -----------------------------------------------------------------------
    // Board
    // -----------------------------------------------------------------------

    /// `B` — swap the main pane for the board, and back.
    fn toggle_board(&mut self) {
        self.board_open = !self.board_open;
        if self.board_open {
            self.focus = Focus::Board;
        } else {
            self.close_board();
        }
    }

    /// Leave the board: back to the chat panes.
    ///
    /// The filter survives, as herdr-board's does within a session — only a
    /// pane restart forgets it, and closing the board is navigating, not
    /// restarting.
    fn close_board(&mut self) {
        self.board_open = false;
        if self.focus == Focus::Board {
            // The composer is where a return to the chat usually lands; if the
            // board was opened from the sidebar, keep the cursor there.
            self.focus = if self.sidebar_visible {
                Focus::Sidebar
            } else {
                Focus::Transcript
            };
        }
    }

    /// `d` — the next board host: automatic, then this device, then each
    /// registered device, then back to automatic (gh#55).
    ///
    /// One key, one direction, and one more press always gets you back to the
    /// default — the same shape `f` gives the filter, and for the same reason:
    /// there is no mode to escape and nothing to undo.
    fn cycle_board_host(&mut self) -> Effects {
        let cycle = self.board_host_cycle();
        let here = cycle.iter().position(|choice| *choice == self.board_pin);
        let next = cycle
            .get(here.map_or(0, |i| i + 1))
            .cloned()
            .unwrap_or(BoardHost::Auto);
        self.board_pin = next.clone();
        self.notify(match &next {
            BoardHost::Auto => "Board host: automatic".to_string(),
            BoardHost::Pinned(None) => "Board host: this device".to_string(),
            BoardHost::Pinned(Some(id)) => format!("Board host: {}", self.device_label(id)),
        });
        vec![Command::BoardHost(next)]
    }

    /// The positions `d` steps through: automatic, this device, then every
    /// registered device in registration order (the order the supervisor's
    /// sweep uses, so the two never disagree about what "next" means).
    fn board_host_cycle(&self) -> Vec<BoardHost> {
        let mut others: Vec<&Device> = self
            .devices
            .iter()
            .filter(|d| Some(d.id.as_str()) != self.local_device_id.as_deref())
            .collect();
        others.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
        let mut cycle = vec![BoardHost::Auto, BoardHost::Pinned(None)];
        cycle.extend(others.into_iter().map(|d| BoardHost::Pinned(Some(d.id.clone()))));
        cycle
    }

    /// What the board header says about where its rows come from. `None` on a
    /// single-device install that found its own board — there is nothing to
    /// tell the operator when there is only one answer.
    pub fn board_host_note(&self) -> Option<String> {
        match (&self.board_host, self.board_host_live) {
            (None, true) => None,
            (None, false) => Some("looking for a board…".to_string()),
            (Some(id), true) => Some(format!("on {}", self.device_label(id))),
            (Some(id), false) => Some(format!("asking {}…", self.device_label(id))),
        }
    }

    /// The release itself, with who is releasing it attached (gh#74): this
    /// device, and the user the engine says is signed in here. The board cannot
    /// verify either — a relayed dispatch reaches the box as the device room's
    /// owner — so they are recorded as the claims they are, and nothing is
    /// authorized on them. Which subscription the run spends is `account`,
    /// always the explicit pick.
    fn dispatch_command(
        &self,
        task_id: String,
        identifier: String,
        account: Option<String>,
    ) -> Command {
        Command::Dispatch {
            task_id,
            identifier,
            account,
            replace: false,
            via_device: self.local_device_id.clone(),
            via_user: self
                .auth_user()
                .map(|user| user.email.clone())
                .filter(|email| !email.is_empty()),
        }
    }

    /// `enter` on the board: dispatch a ready task, fold a section header, or
    /// open a running task's chat.
    fn board_enter(&mut self) -> Effects {
        if let Some(state) = self.board.on_section() {
            self.board.toggle_collapsed(state);
            return Vec::new();
        }
        let Some(row) = self.board.selected_task() else {
            return Vec::new();
        };
        match row.state() {
            // A failed attempt is already closed, so retrying one is an
            // ordinary release — `enter` may do it (gh#73), the desktop rule.
            // A *blocked* row's retry kills a live agent: that stays on `R`.
            // Both go through the account picker — a retry bills someone too.
            BoardState::Ready | BoardState::Failed => {
                if row.dispatchable {
                    // Ask whose subscription this run spends before releasing
                    // it (gh#74). The desktop panel has asked which runtime and
                    // model since it grew a picker; this is the same shape, cut
                    // to the choice that bills someone. Row 0 is the route's own
                    // account, so enter-enter is exactly the old behaviour.
                    let task_id = row.id.clone();
                    let identifier = row.identifier.clone();
                    let runtime = row.runtime.clone();
                    self.overlay = Some(Overlay::DispatchAccount {
                        task_id: task_id.clone(),
                        identifier,
                        accounts: None,
                        route_account: row.account.clone(),
                        harness: None,
                        active: 0,
                    });
                    vec![Command::ListDispatchAccounts { task_id, runtime }]
                } else {
                    self.notify(format!(
                        "{} has no route — it cannot be dispatched",
                        row.identifier
                    ));
                    Vec::new()
                }
            }
            // herdr-board's `g`: a running task's chat is where the work is,
            // and comet's answer to a pane is a chat.
            BoardState::Working | BoardState::Blocked => {
                if let Some(chat_id) = row.chat_id.clone() {
                    self.board_open = false;
                    self.focus = Focus::Composer;
                    self.select_chat(Some(chat_id))
                } else {
                    self.notify(format!("{} is running but has no chat to open", row.identifier));
                    Vec::new()
                }
            }
            _ => {
                self.notify(format!(
                    "{} — {}; enter dispatches a ready task",
                    row.identifier,
                    row.state().as_str()
                ));
                Vec::new()
            }
        }
    }

    /// `R` on the board: retry the selected row (gh#73) — the desktop panel's
    /// Retry chip, which the pane had no answer to.
    ///
    /// On a blocked row this replaces the live attempt: the agent is alive and
    /// waiting on input, so the one-live-attempt rule would refuse a plain
    /// release, and ending it is what a retry means. On a failed or ready row
    /// nothing is live and it is an ordinary dispatch. Anything else keeps its
    /// attempt and says so rather than quietly cancelling someone's work.
    fn board_retry(&mut self) -> Effects {
        if self.board.on_section().is_some() {
            return Vec::new();
        }
        let Some(row) = self.board.selected_task() else {
            return Vec::new();
        };
        match row.state() {
            BoardState::Blocked => self.board_release(true),
            BoardState::Ready | BoardState::Failed => self.board_release(false),
            state => {
                self.notify(format!(
                    "{} — {}; R retries a blocked or failed task",
                    row.identifier,
                    state.as_str()
                ));
                Vec::new()
            }
        }
    }

    /// Release the selected row, replacing its live attempt or not. The route
    /// check is here rather than at each caller: an unrouted row cannot be
    /// dispatched however you asked for it.
    fn board_release(&mut self, replace: bool) -> Effects {
        let Some(row) = self.board.selected_task() else {
            return Vec::new();
        };
        if !row.dispatchable {
            let identifier = row.identifier.clone();
            self.notify(format!(
                "{identifier} has no route — it cannot be dispatched"
            ));
            return Vec::new();
        }
        vec![Command::Dispatch {
            task_id: row.id.clone(),
            identifier: row.identifier.clone(),
            replace,
            account: None,
            via_device: self.local_device_id.clone(),
            via_user: self
                .auth_user()
                .map(|user| user.email.clone())
                .filter(|email| !email.is_empty()),
        }]
    }

    // -----------------------------------------------------------------------
    // Selection
    // -----------------------------------------------------------------------

    fn open_row(&mut self) -> Effects {
        match self.rows.get(self.cursor).cloned() {
            Some(Row::Space { id, .. }) => self.activate_space(id),
            // Decoration isn't selectable, so nothing else can be under the
            // cursor; a defensive arm keeps this total.
            Some(Row::Blank)
            | Some(Row::Section { .. })
            | Some(Row::Empty { .. })
            | Some(Row::User { .. })
            | None => Vec::new(),
            Some(Row::Chat { id, space_id, .. }) => {
                if space_id.is_some() {
                    self.selected_space = space_id.clone();
                }
                let effects = self.select_chat(Some(id));
                self.focus = Focus::Composer;
                effects
            }
            // An agent row is its chat, said another way: opening it opens the
            // transcript, which is where answering a blocked agent happens. The
            // chat belongs to whatever space the dispatch routed to, so landing
            // there is part of opening it.
            Some(Row::Agent { chat_id, .. }) => {
                if let Some(space) = self
                    .chats
                    .iter()
                    .find(|chat| chat.id == chat_id)
                    .and_then(|chat| chat.space_id.clone())
                {
                    self.selected_space = Some(space);
                }
                let effects = self.select_chat(Some(chat_id));
                self.focus = Focus::Composer;
                effects
            }
            // Same again for an unmanaged run, which already carries the space
            // it belongs to: opening it is opening its transcript, which is
            // where answering it happens.
            Some(Row::Running {
                chat_id, space_id, ..
            }) => {
                if space_id.is_some() {
                    self.selected_space = space_id.clone();
                }
                let effects = self.select_chat(Some(chat_id));
                self.focus = Focus::Composer;
                effects
            }
        }
    }

    /// Switch to a space and go where you left off in it: the last session you
    /// had open there, else its most recent one, else a fresh draft — matching
    /// the desktop app, where a space is a place rather than a filter.
    pub fn activate_space(&mut self, space_id: String) -> Effects {
        self.selected_space = Some(space_id.clone());
        self.draft = None;

        let remembered = self
            .last_visited
            .get(&space_id)
            .filter(|id| {
                self.chats.iter().any(|chat| {
                    &&chat.id == id && !chat.archived && chat.space_id.as_deref() == Some(&space_id)
                })
            })
            .cloned();
        let target = remembered.or_else(|| {
            // `chats` is recency-sorted, so the first match is the newest.
            self.chats
                .iter()
                .find(|chat| !chat.archived && chat.space_id.as_deref() == Some(space_id.as_str()))
                .map(|chat| chat.id.clone())
        });

        match target {
            Some(chat_id) => {
                let effects = self.select_chat(Some(chat_id));
                self.rebuild_rows();
                effects
            }
            // An empty space opens the new-session canvas: there is nothing to
            // return to, and a blank pane would be a dead end.
            None => self.new_session_in(space_id),
        }
    }

    /// Point the transcript at a chat, subscribing its doc and clearing its
    /// unseen badge.
    pub fn select_chat(&mut self, chat_id: Option<String>) -> Effects {
        if self.selected_chat == chat_id {
            return Vec::new();
        }
        // A draft and an open session are mutually exclusive: opening a real
        // session abandons the draft. Leaving both set is what made a send in
        // an existing session create a new one instead.
        if chat_id.is_some() {
            self.draft = None;
        }
        self.selected_chat = chat_id.clone();
        // Remember it against its space, for the next time you come back.
        if let Some(id) = chat_id.as_deref()
            && let Some(space) = self
                .chats
                .iter()
                .find(|chat| chat.id == id)
                .and_then(|chat| chat.space_id.clone())
        {
            self.last_visited.insert(space, id.to_string());
        }
        self.transcript.retarget(chat_id.clone());
        self.transcript_stale = true;
        let mut effects = vec![Command::WatchTranscript(chat_id.clone())];
        // Clear the "completed (unseen)" badge everywhere — the marker is a
        // synced LWW field, so reading here reads on every device.
        if let Some(chat_id) = chat_id
            && self
                .chats
                .iter()
                .any(|chat| chat.id == chat_id && chat.unseen())
        {
            effects.push(Command::Call {
                method: methods::MUTATE,
                params: serde_json::json!({ "op": "markChatSeen", "chatId": chat_id }),
                context: "Couldn't mark the session read",
            });
        }
        effects
    }

    /// Keep `selected_space` pointing at a row that exists, and default it to
    /// the first space so the main pane is never blank while spaces exist.
    fn heal_space_selection(&mut self) {
        if let Some(selected) = &self.selected_space
            && !self.spaces.iter().any(|space| &space.id == selected)
        {
            self.selected_space = None;
        }
        if self.selected_space.is_none() {
            self.selected_space = self.spaces.first().map(|space| space.id.clone());
        }
    }

    /// Drop a selection whose chat vanished (deleted on another device), and
    /// auto-select the most recent one on the first frame that has any.
    fn heal_chat_selection(&mut self) -> Effects {
        // A draft *is* the selection: it has no chat id by design, so the
        // "nothing selected, pick something" rule below does not apply to it.
        // Without this, every chats frame — and they arrive constantly while
        // any session streams — yanked you off the new-session canvas.
        if self.draft.is_some() {
            return Vec::new();
        }
        // A chat we just asked the engine to create isn't in `chats` yet, and a
        // watch frame can easily land in that window. Healing it would yank the
        // user off the session they just opened and back onto the previous one —
        // so the pending id is exempt until it appears (or the wait times out,
        // which is how a *failed* createChat still recovers).
        if let Some((id, since)) = &self.pending_chat {
            // Settled either way once the row lands or the wait runs out.
            let settled = self.chats.iter().any(|chat| &chat.id == id)
                || since.elapsed() > PENDING_CHAT_GRACE;
            if settled {
                self.pending_chat = None;
            } else if self.selected_chat.as_deref() == Some(id.as_str()) {
                return Vec::new();
            }
        }
        if let Some(selected) = self.selected_chat.clone()
            && !self.chats.iter().any(|chat| chat.id == selected)
        {
            return self.select_chat(None);
        }
        if self.selected_chat.is_none() {
            // `chats` is recency-sorted, so the first visible row is the one the
            // user most likely wants open.
            if let Some(chat) = self.chats.iter().find(|chat| !chat.archived) {
                let id = chat.id.clone();
                let space = chat.space_id.clone();
                if space.is_some() {
                    self.selected_space = space;
                }
                let effects = self.select_chat(Some(id.clone()));
                self.cursor = self
                    .rows
                    .iter()
                    .position(|row| row.id() == Some(id.as_str()))
                    .unwrap_or(self.cursor);
                return effects;
            }
        }
        Vec::new()
    }

    // -----------------------------------------------------------------------
    // Mutations
    // -----------------------------------------------------------------------

    /// Begin composing a session. Nothing is created yet — the desktop app
    /// opens a canvas and materializes the tab on the first send, and so does
    /// this.
    fn new_session(&mut self) -> Effects {
        let Some(space_id) = self.space_for_new_session() else {
            self.notify(
                "No space to create a session in — add one from the desktop app first.".into(),
            );
            return Vec::new();
        };
        self.new_session_in(space_id)
    }

    /// Begin composing in a *named* space. `activate_space` needs this: falling
    /// back to whatever the cursor happens to be on would open the draft in the
    /// wrong place.
    fn new_session_in(&mut self, space_id: String) -> Effects {
        self.selected_space = Some(space_id.clone());
        self.draft = Some(Draft {
            space_id: space_id.clone(),
            checkout: CheckoutKind::default(),
            picked_ref: None,
            refs: None,
            model: None,
            reasoning: None,
        });
        self.focus = Focus::Composer;
        // Drop the open session: the draft owns the main pane until it is sent
        // or abandoned.
        let mut effects = self.select_chat(None);
        self.rebuild_rows();
        if let Some(space) = self.spaces.iter().find(|space| space.id == space_id)
            && space.git_detected
        {
            effects.push(Command::ListRefs {
                repo_path: space.path.clone(),
                target_device: self.remote_device(&space.device_id),
            });
        }
        effects
    }

    /// The device id to address a call to, or `None` when the space is hosted
    /// here (the engine refuses to forward a call to itself).
    fn remote_device(&self, device_id: &str) -> Option<String> {
        match self.local_device_id.as_deref() {
            Some(local) if local == device_id => None,
            _ => Some(device_id.to_string()),
        }
    }

    /// The draft's space, if it still exists.
    fn draft_space(&self) -> Option<&Space> {
        let draft = self.draft.as_ref()?;
        self.spaces.iter().find(|space| space.id == draft.space_id)
    }

    /// The space a new session belongs to: the one under the cursor, else the
    /// selected one, else the first.
    fn space_for_new_session(&self) -> Option<String> {
        match self.rows.get(self.cursor) {
            Some(Row::Space { id, .. }) => return Some(id.clone()),
            Some(Row::Chat {
                space_id: Some(id), ..
            }) => return Some(id.clone()),
            _ => {}
        }
        self.selected_space
            .clone()
            .or_else(|| self.spaces.first().map(|space| space.id.clone()))
    }

    fn toggle_archive(&mut self) -> Effects {
        let Some(Row::Chat { id, archived, .. }) = self.rows.get(self.cursor) else {
            return Vec::new();
        };
        let (chat_id, archived) = (id.clone(), *archived);
        vec![Command::Call {
            method: methods::MUTATE,
            params: serde_json::json!({
                "op": "setChatArchived",
                "chatId": chat_id,
                "archived": !archived,
            }),
            context: "Couldn't archive the session",
        }]
    }

    /// Pin a chat as the board's orchestrator, or unpin whatever is (gh#104).
    ///
    /// One `[defaults]` key on the board's `routing.toml`, written through the
    /// same validated path a settings page uses. `None` removes the key, which
    /// is the kill switch: the notices stop and the chat is an ordinary chat
    /// again. The pin is not applied optimistically — the board republishes it
    /// as the write lands, and a refusal must not leave a glyph on a row the
    /// box does not agree about.
    fn set_orchestrator(&mut self, chat_id: Option<String>) -> Effects {
        if self.board_host.is_none() && !self.board_host_live {
            self.notify("No board found yet — nothing to pin an orchestrator on.".into());
            return Vec::new();
        }
        let mut params = serde_json::json!({ "op": "default", "key": "orchestrator_chat" });
        if let Some(object) = params.as_object_mut() {
            if let Some(id) = &chat_id {
                object.insert("value".into(), serde_json::json!(id));
            }
            if let Some(device) = &self.board_host {
                object.insert("targetDeviceId".into(), serde_json::json!(device));
            }
        }
        vec![Command::Call {
            method: methods::WRITE_BOARD_CONFIG,
            params,
            context: "Couldn't change the board's orchestrator",
        }]
    }

    fn interrupt(&mut self) -> Effects {
        let Some(chat_id) = self.selected_chat.clone() else {
            return Vec::new();
        };
        let command = match serde_json::to_value(SessionCommandPayload::Interrupt {}) {
            Ok(value) => value,
            Err(_) => return Vec::new(),
        };
        vec![Command::Call {
            method: methods::QUEUE_COMMAND,
            params: serde_json::json!({ "chatId": chat_id, "command": command }),
            context: "Couldn't interrupt the run",
        }]
    }

    /// Queue the composer's text. Mirrors the gpui composer's send path: a
    /// `Steer` while the agent is mid-turn (the harness folds it into the live
    /// run), a `Run` otherwise, both carrying a client-minted message id that
    /// doubles as the echo's dedupe key.
    fn send(&mut self) -> Effects {
        if self.composer.is_empty() {
            return Vec::new();
        }
        self.debug_check_selection();
        if self.draft.is_some() {
            return self.send_draft();
        }
        let Some(chat_id) = self.selected_chat.clone() else {
            self.notify("Open or create a session first (n).".into());
            return Vec::new();
        };
        let prompt = self.composer.take();
        let message_id = uuid::Uuid::new_v4().to_string();
        let created_at = Utc::now().timestamp_millis();

        let working = matches!(
            self.indicator_for(&chat_id),
            Indicator::Working | Indicator::AwaitingInput
        );
        let payload = if working {
            SessionCommandPayload::Steer {
                prompt: prompt.clone(),
                message_id: Some(message_id.clone()),
            }
        } else {
            let chat = self.chats.iter().find(|chat| chat.id == chat_id);
            let config = chat.and_then(|chat| chat.config.clone());
            SessionCommandPayload::Run {
                request: RunRequest {
                    prompt: prompt.clone(),
                    model: config.as_ref().and_then(|c| c.model.clone()),
                    reasoning: config.as_ref().and_then(|c| c.reasoning),
                    model_options: config
                        .as_ref()
                        .map(|c| c.model_options.clone())
                        .unwrap_or_default(),
                    cwd: self.cwd_for(&chat_id),
                    sandbox: config
                        .as_ref()
                        .map(|c| c.sandbox)
                        .unwrap_or(SandboxLevel::WorkspaceWrite),
                    auto_approve: false,
                    // Resume continuity is engine-owned (it stamps
                    // `harnessSessionId` on the chat row); a client that guessed
                    // here would fight it.
                    resume: None,
                    attachments: Vec::new(),
                },
                message_id: message_id.clone(),
            }
        };

        let Ok(command) = serde_json::to_value(&payload) else {
            self.composer.set_text(prompt);
            self.notify("Couldn't encode the command.".into());
            return Vec::new();
        };

        self.push_echo(
            &chat_id,
            SessionMessageEntry {
                id: message_id.clone(),
                role: MessageRole::User,
                parts: vec![MessagePart::Text {
                    id: "t0".into(),
                    text: prompt,
                }],
                created_at,
                device_id: "local".into(),
                status: None,
                continuation_of: None,
            },
        );
        // Sending always jumps to the bottom: you want to watch what you asked
        // for, even if you were reading history.
        self.transcript.to_bottom();
        self.focus = Focus::Composer;

        vec![Command::Send {
            chat_id: chat_id.clone(),
            message_id,
            params: serde_json::json!({ "chatId": chat_id, "command": command }),
        }]
    }

    /// The two selection states are mutually exclusive; nothing should ever
    /// hold both. Checked in debug builds at every send, which is where holding
    /// both actually hurts.
    fn debug_check_selection(&self) {
        debug_assert!(
            !(self.draft.is_some() && self.selected_chat.is_some()),
            "draft and selected_chat both set: a send would create a session \
             instead of continuing {:?}",
            self.selected_chat
        );
    }

    /// First send on a draft: materialize the session where the checkout choice
    /// says it should live, then queue the prompt. One command, because the
    /// steps are ordered and dependent (see [`crate::link::StartSession`]).
    fn send_draft(&mut self) -> Effects {
        let Some(draft) = self.draft.clone() else {
            return Vec::new();
        };
        let Some(space) = self.draft_space().cloned() else {
            self.notify("That space is gone.".into());
            self.draft = None;
            return Vec::new();
        };

        let prompt = self.composer.take();
        let chat_id = uuid::Uuid::new_v4().to_string();
        let message_id = uuid::Uuid::new_v4().to_string();
        let created_at = Utc::now().timestamp_millis();
        let plan = draft.plan();

        // The cwd is provisional: a NewWorktree plan only learns its path once
        // the engine has made it, so the link overwrites this before queueing.
        let cwd = match &plan {
            CheckoutPlan::ReuseWorktree { path, .. } => path.clone(),
            _ => space.path.clone(),
        };
        let payload = SessionCommandPayload::Run {
            request: RunRequest {
                prompt: prompt.clone(),
                model: None,
                reasoning: None,
                model_options: Default::default(),
                cwd,
                sandbox: SandboxLevel::WorkspaceWrite,
                auto_approve: false,
                resume: None,
                attachments: Vec::new(),
            },
            message_id: message_id.clone(),
        };
        let Ok(command) = serde_json::to_value(&payload) else {
            self.composer.set_text(prompt);
            self.notify("Couldn't encode the command.".into());
            return Vec::new();
        };

        self.push_echo(
            &chat_id,
            SessionMessageEntry {
                id: message_id.clone(),
                role: MessageRole::User,
                parts: vec![MessagePart::Text {
                    id: "t0".into(),
                    text: prompt,
                }],
                created_at,
                device_id: "local".into(),
                status: None,
                continuation_of: None,
            },
        );
        // Show the echo under the id the session will have, so the transcript
        // does not blink when the row arrives.
        self.draft = None;
        self.pending_chat = Some((chat_id.clone(), std::time::Instant::now()));
        let mut effects = self.select_chat(Some(chat_id.clone()));
        self.transcript.to_bottom();
        self.rebuild_rows();

        // The draft's model/effort become the chat's config, so the first run
        // starts on the settings you picked rather than the harness default.
        let config = draft.model.as_ref().map(|model| comet_proto::ChatConfig {
            harness: comet_proto::HarnessId::ClaudeCode,
            model: Some(model.id.clone()),
            reasoning: draft.reasoning,
            model_options: Default::default(),
            sandbox: SandboxLevel::WorkspaceWrite,
            account: None,
            push_repo: None,
            git_author: None,
        });
        effects.push(Command::StartSession(Box::new(crate::link::StartSession {
            chat_id,
            space_id: space.id.clone(),
            repo_path: space.path.clone(),
            target_device: self.remote_device(&space.device_id),
            plan,
            config: config.and_then(|c| serde_json::to_value(&c).ok()),
            message_id,
            command,
        })));
        effects
    }

    /// Working directory for a run: the chat's own cwd (an isolated worktree),
    /// else its space's folder.
    fn cwd_for(&self, chat_id: &str) -> String {
        let chat = self.chats.iter().find(|chat| chat.id == chat_id);
        if let Some(cwd) = chat
            .and_then(|chat| chat.cwd.clone())
            .filter(|c| !c.is_empty())
        {
            return cwd;
        }
        let space_id = chat
            .and_then(|chat| chat.space_id.clone())
            .or_else(|| self.selected_space.clone());
        space_id
            .and_then(|id| self.spaces.iter().find(|space| space.id == id))
            .map(|space| space.path.clone())
            .unwrap_or_default()
    }

    // -----------------------------------------------------------------------
    // Derived views
    // -----------------------------------------------------------------------

    /// Rebuild the flattened sidebar, preserving the cursor's *row identity*
    /// rather than its index — otherwise a session appearing above the cursor
    /// would silently move the selection under the user's hands.
    ///
    /// Three sections, matching the desktop shell: Spaces, the live Agents (only
    /// while something is running), then a flat global attention-sorted Sessions
    /// list.
    pub fn rebuild_rows(&mut self) {
        let anchor = self.rows.get(self.cursor).and_then(|row| row.key());
        let now = Utc::now();
        let user_row = self.auth_user().map(|user| {
            (
                user.name
                    .clone()
                    .filter(|n| !n.trim().is_empty())
                    .unwrap_or_else(|| user.email.clone()),
                user.email.clone(),
            )
        });

        // Aggregate attention per space: the most urgent live member wins, so a
        // running session stays visible when the Sessions list is scrolled off.
        let mut attention: HashMap<String, ChatIndicator> = HashMap::new();
        for chat in self.chats.iter().filter(|chat| !chat.archived) {
            let status = display_status(chat, self.session_for(&chat.id), now);
            if !matches!(
                status,
                ChatIndicator::Working | ChatIndicator::AwaitingInput
            ) {
                continue;
            }
            let Some(space_id) = chat.space_id.clone() else {
                continue;
            };
            attention
                .entry(space_id)
                .and_modify(|held| {
                    if view::attention_rank(status) < view::attention_rank(*held) {
                        *held = status;
                    }
                })
                .or_insert(status);
        }

        let mut rows = Vec::with_capacity(self.spaces.len() + self.chats.len() + 6);
        rows.push(Row::Section {
            label: "Spaces".into(),
            action: Some("+".into()),
        });
        if self.spaces.is_empty() {
            rows.push(Row::Empty {
                label: "No spaces yet".into(),
            });
        }
        for space in &self.spaces {
            rows.push(Row::Space {
                id: space.id.clone(),
                label: space.display_name().to_string(),
                device: self.device_label(&space.device_id),
                attention: attention.get(&space.id).copied(),
                offline: !self.device_online(&space.device_id, now),
            });
        }

        // Agents, above the sessions: one row per live board attempt (gh#103).
        // Omitted entirely when nothing is running — an empty section here would
        // be a permanent reminder that a board exists on a box that has none.
        // The board pane (`B`) stays the deep view; this is the glance.
        let agents = board_view::agent_rows(&self.board.rows, &self.chats, &self.sessions, now);
        if !agents.is_empty() {
            let blocked = board_view::agents_needing_attention(&agents);
            rows.push(Row::Blank);
            rows.push(Row::Section {
                label: "Agents".into(),
                // The count is on the header because it is what you look for
                // first: three running, one of them stuck on a question.
                action: (blocked > 0).then(|| format!("{blocked} blocked")),
            });
            for agent in agents {
                rows.push(Row::Agent {
                    chat_id: agent.chat_id,
                    identifier: agent.identifier,
                    branch: agent.branch,
                    state: agent.state,
                    started_at: agent.started_at,
                    cap_secs: agent.cap_secs,
                });
            }
        }

        // Running, under them: everything working that the board did NOT
        // release (gh#117) — the pinned orchestrator, an ad-hoc agent chat, a
        // session somebody started by hand. Same shape, separate group, because
        // these have no issue, branch, cap or bill behind them; the board rows
        // are read only to subtract the attempts, so a box hosting no board
        // fills this group all the same.
        let running = board_view::running_rows(&self.board.rows, &self.chats, &self.sessions, now);
        if !running.is_empty() {
            let blocked = board_view::running_needing_attention(&running);
            rows.push(Row::Blank);
            rows.push(Row::Section {
                label: "Running".into(),
                action: (blocked > 0).then(|| format!("{blocked} blocked")),
            });
            for run in running {
                rows.push(Row::Running {
                    chat_id: run.chat_id,
                    space_id: run.space_id,
                    title: run.title,
                    state: run.state,
                    started_at: run.started_at,
                });
            }
        }

        rows.push(Row::Blank);
        rows.push(Row::Section {
            label: "Sessions".into(),
            action: self.show_archived.then(|| "archived".to_string()),
        });
        // Flat and global, ordered by `sort_active` — the same recency order the
        // desktop list uses, so the two surfaces agree row for row.
        let mut active: Vec<(ChatIndicator, &Chat)> = self
            .chats
            .iter()
            .filter(|chat| self.show_archived || !chat.archived)
            .map(|chat| (display_status(chat, self.session_for(&chat.id), now), chat))
            .collect();
        view::sort_active(&mut active);
        // ...and then the orchestrator on top of it, if this board has one. It
        // is pinned in the literal sense: recency decides every other row's
        // position and this one's position is the designation.
        if let Some(pinned) = self.orchestrator.as_deref()
            && let Some(at) = active.iter().position(|(_, chat)| chat.id == pinned)
        {
            let row = active.remove(at);
            active.insert(0, row);
        }
        if active.is_empty() {
            rows.push(Row::Empty {
                label: "No sessions yet".into(),
            });
        }
        for (indicator, chat) in active {
            rows.push(Row::Chat {
                id: chat.id.clone(),
                space_id: chat.space_id.clone(),
                title: chat
                    .title
                    .clone()
                    .filter(|title| !title.trim().is_empty())
                    .unwrap_or_else(|| "New session".to_string()),
                location: self.space_at_device(chat.space_id.as_deref()),
                indicator,
                archived: chat.archived,
                activity: chat.last_message_at.or(Some(chat.created_at)),
                orchestrator: self.orchestrator.as_deref() == Some(chat.id.as_str()),
            });
        }

        if let Some((name, email)) = user_row {
            // Air, not a hairline: the user row is already pinned to the bottom
            // of the pane, so a rule above it is a second answer to a question
            // position has already answered.
            rows.push(Row::Blank);
            rows.push(Row::User { name, email });
        }

        self.rows = rows;
        self.cursor = anchor
            .and_then(|key| {
                self.rows
                    .iter()
                    .position(|row| row.key().as_deref() == Some(key.as_str()))
            })
            .or_else(|| {
                self.selected_chat
                    .as_deref()
                    .and_then(|id| self.rows.iter().position(|row| row.id() == Some(id)))
            })
            .unwrap_or_else(|| self.first_selectable());
        // A rebuild can land the cursor on decoration (its row vanished); walk
        // to the nearest row that can actually hold it.
        if !self.rows.get(self.cursor).is_some_and(Row::selectable) {
            self.cursor = self
                .next_selectable(self.cursor, 1)
                .or_else(|| self.next_selectable(self.cursor, -1))
                .unwrap_or(0);
        }
    }

    /// The selected space's non-archived sessions, as the tab strip above the
    /// transcript. This is where a space's own sessions live in comet-native —
    /// the sidebar's Sessions list is global and answers a different question.
    ///
    /// Tab order is creation order (`sort_tabs`): activity must never reorder
    /// tabs under the pointer.
    pub fn tabs(&self) -> Vec<Tab> {
        let Some(space_id) = self.selected_space.as_deref() else {
            return Vec::new();
        };
        let drafting = self
            .draft
            .as_ref()
            .is_some_and(|draft| draft.space_id == space_id);
        let now = Utc::now();
        let mut chats: Vec<&Chat> = self
            .chats
            .iter()
            .filter(|chat| chat.space_id.as_deref() == Some(space_id))
            .filter(|chat| !chat.archived)
            .collect();
        view::sort_tabs(&mut chats);
        let mut tabs: Vec<Tab> = chats
            .into_iter()
            .map(|chat| Tab {
                id: chat.id.clone(),
                title: chat
                    .title
                    .clone()
                    .filter(|title| !title.trim().is_empty())
                    .unwrap_or_else(|| "New session".to_string()),
                indicator: display_status(chat, self.session_for(&chat.id), now),
                active: !drafting && self.selected_chat.as_deref() == Some(chat.id.as_str()),
            })
            .collect();
        if drafting {
            tabs.push(Tab {
                id: String::new(),
                title: "New session".into(),
                indicator: ChatIndicator::Idle,
                active: true,
            });
        }
        tabs
    }

    /// Select the tab `step` away from the active one, wrapping.
    pub fn cycle_tab(&mut self, step: isize) -> Effects {
        let tabs = self.tabs();
        if tabs.is_empty() {
            return Vec::new();
        }
        let active = tabs.iter().position(|tab| tab.active).unwrap_or(0) as isize;
        let next = (active + step).rem_euclid(tabs.len() as isize) as usize;
        let id = tabs[next].id.clone();
        let effects = self.select_chat(Some(id));
        self.rebuild_rows();
        effects
    }

    /// Select the tab at a 1-based index, if it exists.
    pub fn select_tab(&mut self, index: usize) -> Effects {
        let tabs = self.tabs();
        let Some(tab) = index.checked_sub(1).and_then(|i| tabs.get(i)) else {
            return Vec::new();
        };
        let id = tab.id.clone();
        let effects = self.select_chat(Some(id));
        self.rebuild_rows();
        effects
    }

    /// A session's sub-line: `space@device` — where the work actually happens.
    /// The branch lives on the session itself and changes mid-run; the machine
    /// and folder are the stable facts worth carrying under every title.
    fn space_at_device(&self, space_id: Option<&str>) -> Option<String> {
        let space = self
            .spaces
            .iter()
            .find(|space| Some(space.id.as_str()) == space_id)?;
        Some(format!(
            "{}@{}",
            space.display_name(),
            self.device_label(&space.device_id)
        ))
    }

    /// A device's display name, falling back to its id's head when the registry
    /// has not arrived (or the device is another user's).
    fn device_label(&self, device_id: &str) -> String {
        if self.local_device_id.as_deref() == Some(device_id) {
            return "this device".to_string();
        }
        self.devices
            .iter()
            .find(|device| device.id == device_id)
            .map(|device| device.name.clone())
            .unwrap_or_else(|| device_id.chars().take(8).collect())
    }

    /// Host presence: a space whose device's heartbeat lapsed reads offline —
    /// a host outage, not slow sync.
    fn device_online(&self, device_id: &str, now: DateTime<Utc>) -> bool {
        if self.local_device_id.as_deref() == Some(device_id) {
            return true;
        }
        match self
            .devices
            .iter()
            .find(|device| device.id == device_id)
            .and_then(|device| device.last_seen_at)
        {
            Some(seen) => {
                // The shared presence window — the gpui app's devices page and
                // space rows use the same one, so the two surfaces can never
                // disagree about the same device (gh#126).
                now.signed_duration_since(seen).num_milliseconds() <= view::PRESENCE_STALE_MS
            }
            // No record yet (registry still arriving, or another user's device):
            // say nothing rather than claim an outage we cannot see.
            None => true,
        }
    }

    fn first_selectable(&self) -> usize {
        self.rows.iter().position(Row::selectable).unwrap_or(0)
    }

    /// The next selectable row in `step` direction, skipping decoration.
    fn next_selectable(&self, from: usize, step: isize) -> Option<usize> {
        let mut at = from as isize;
        loop {
            at += step;
            if at < 0 || at as usize >= self.rows.len() {
                return None;
            }
            if self.rows[at as usize].selectable() {
                return Some(at as usize);
            }
        }
    }

    /// Elapsed time of the selected chat's live run, for the status strip.
    pub fn working_elapsed(&self) -> Option<std::time::Duration> {
        let chat_id = self.selected_chat.as_deref()?;
        let session = self.session_for(chat_id)?;
        if view::effective_indicator(Some(session), Utc::now()) != Indicator::Working {
            return None;
        }
        let started = session.started_at.unwrap_or(session.updated_at);
        Utc::now()
            .signed_duration_since(started)
            .to_std()
            .ok()
            .or(Some(std::time::Duration::ZERO))
    }

    /// The composer's meta row: which model will answer, and at what effort.
    ///
    /// This is the desktop's single combined model+effort chip (`pickers.rs`,
    /// `combined_chip`) and it follows that chip's central rule — **it is never
    /// absent**. The desktop resolves the label down a chain (loaded model →
    /// remembered default → the configured id) precisely so the toolbar never
    /// blinks through an empty state while a session's config is in flight; the
    /// note there reads "Never 'Default model'". The TUI had the opposite
    /// behaviour: no config yet meant no chips at all, so tabbing between
    /// sessions made the whole row appear and disappear.
    ///
    /// Effort is the only optional half, and it costs nothing when missing —
    /// it is a suffix inside one row, not a row of its own.
    pub fn composer_meta(&self) -> ComposerMeta {
        let (model, reasoning) = match &self.draft {
            Some(draft) => (
                draft.model.as_ref().map(|model| model.label.clone()),
                draft.reasoning,
            ),
            None => match self
                .selected_chat_row()
                .and_then(|chat| chat.config.as_ref())
            {
                Some(config) => (
                    config
                        .model
                        .clone()
                        .filter(|model| !model.trim().is_empty()),
                    config.reasoning,
                ),
                None => (None, None),
            },
        };
        ComposerMeta {
            model: model.unwrap_or_else(|| "Model".into()),
            effort: reasoning.map(|level| reasoning_label(level).to_string()),
        }
    }

    /// The composer's footer: the ref this work sits on, and where it runs.
    ///
    /// Ported from the desktop's `render_footer`, including the three rules that
    /// made it stop flickering there:
    ///
    /// 1. **Git gates the whole row.** A space that is not a checkout has no ref
    ///    and no worktree, so the row is absent rather than empty.
    /// 2. **Both modes get it.** The ref side is live for a draft *and* for an
    ///    open session — t3code keeps its branch selector interactive mid-session
    ///    and so does comet. The TUI previously showed this only while drafting,
    ///    which is why it vanished the moment you opened a tab.
    /// 3. **A selected chat whose row hasn't synced yet still renders the draft
    ///    footer.** The values are identical, so the toolbar never blinks through
    ///    a half-empty locked state in the moment right after send mints a
    ///    session.
    ///
    /// The checkout side differs by mode: a session's checkout kind is fixed at
    /// creation (harness resume is cwd-scoped — the session never moves folders),
    /// so it is a label there and a button only on a draft.
    pub fn composer_footer(&self) -> Option<ComposerFooter> {
        let space = self.selected_space_row()?;
        if !space.git_detected {
            return None;
        }
        // Rule 3: `selected_chat_row` is None for a chat that exists but has not
        // synced, and that falls through to the draft arm on purpose.
        let session = self
            .selected_chat
            .as_ref()
            .and_then(|_| self.selected_chat_row());
        Some(match session {
            Some(chat) => ComposerFooter {
                reference: chat
                    .branch
                    .clone()
                    .unwrap_or_else(|| "Select ref".to_string()),
                checkout: if chat.cwd.as_deref().is_some_and(|cwd| cwd != space.path) {
                    "Worktree".to_string()
                } else {
                    "Current checkout".to_string()
                },
                checkout_live: false,
            },
            None => ComposerFooter {
                reference: self
                    .draft
                    .as_ref()
                    .map(Draft::ref_label)
                    .unwrap_or_else(|| "Select ref".to_string()),
                checkout: self
                    .draft
                    .as_ref()
                    .map(|draft| draft.checkout_label().to_string())
                    .unwrap_or_else(|| "Current checkout".to_string()),
                checkout_live: true,
            },
        })
    }

    /// The space the sidebar has selected.
    pub fn selected_space_row(&self) -> Option<&comet_proto::Space> {
        let id = self.selected_space.as_deref()?;
        self.spaces.iter().find(|space| space.id == id)
    }

    /// The effort levels the active model offers, or a sensible default set
    /// when no catalogue entry is known.
    fn reasoning_levels(&self) -> Vec<comet_proto::ReasoningLevel> {
        use comet_proto::ReasoningLevel as R;
        if let Some(model) = self.draft.as_ref().and_then(|draft| draft.model.as_ref())
            && !model.reasoning_levels.is_empty()
        {
            return model.reasoning_levels.clone();
        }
        vec![R::Minimal, R::Low, R::Medium, R::High]
    }

    /// Open the effort picker for the active model.
    pub fn open_reasoning_picker(&mut self) -> Effects {
        if self.draft.is_none() && self.selected_chat_row().is_none() {
            self.notify("Open a session first.".into());
            return Vec::new();
        }
        let levels = self.reasoning_levels();
        let current = match &self.draft {
            Some(draft) => draft.reasoning,
            None => self
                .selected_chat_row()
                .and_then(|chat| chat.config.as_ref())
                .and_then(|config| config.reasoning),
        };
        let active = current
            .and_then(|level| levels.iter().position(|candidate| *candidate == level))
            .unwrap_or(0);
        self.overlay = Some(Overlay::Reasoning { levels, active });
        Vec::new()
    }

    /// How long the viewport has been up — the loaders' clock.
    pub fn elapsed(&self) -> std::time::Duration {
        self.started.elapsed()
    }

    pub fn gate(&self) -> GatePhase {
        view::gate_phase(&self.connection, self.auth.as_ref())
    }

    pub fn session_for(&self, chat_id: &str) -> Option<&Session> {
        self.sessions
            .iter()
            .find(|session| session.chat_id == chat_id)
    }

    /// Staleness-gated live status for a chat.
    pub fn indicator_for(&self, chat_id: &str) -> Indicator {
        view::effective_indicator(self.session_for(chat_id), Utc::now())
    }

    pub fn selected_chat_row(&self) -> Option<&Chat> {
        let id = self.selected_chat.as_deref()?;
        self.chats.iter().find(|chat| chat.id == id)
    }

    /// The signed-in user, when the engine reports one.
    pub fn auth_user(&self) -> Option<&comet_proto::UserProfile> {
        match self.auth.as_ref()? {
            AuthState::SignedIn { user, .. } | AuthState::NeedsOrganization { user } => Some(user),
            AuthState::SignedOut => None,
        }
    }

    /// True while something on screen is animating: a live session, or the boot
    /// gate's comet mark. The event loop only arms its timer when this holds,
    /// so an idle, connected app costs zero wakeups.
    pub fn animating(&self) -> bool {
        if !matches!(self.gate(), GatePhase::Ready) {
            return true;
        }
        let now = Utc::now();
        self.sessions.iter().any(|session| {
            matches!(
                view::effective_indicator(Some(session), now),
                Indicator::Working
            )
        })
    }

    /// A second-resolution counter is on screen: the sidebar's Agents or
    /// Running section (or the board pane) is showing elapsed times, and they
    /// have to move.
    ///
    /// Separate from [`App::animating`], which paces the spinner at animation
    /// framerate. This wants one wake-up a second, and only while something is
    /// actually live — a *blocked* row animates nothing and would otherwise sit
    /// at the age it had when the last frame happened to land.
    pub fn counting(&self) -> bool {
        matches!(self.gate(), GatePhase::Ready)
            && self.rows.iter().any(|row| {
                matches!(
                    row,
                    Row::Agent {
                        started_at: Some(_),
                        ..
                    } | Row::Running {
                        started_at: Some(_),
                        ..
                    }
                )
            })
    }

    /// Relative activity label for a sidebar row.
    pub fn relative_time(&self, at: Option<DateTime<Utc>>) -> String {
        at.map(|at| format_time_ago(at, Utc::now()))
            .unwrap_or_default()
    }

    pub fn notify(&mut self, text: String) {
        self.notice = Some(Notice {
            text,
            until: std::time::Instant::now() + NOTICE_TTL,
        });
    }

    /// When the event loop must wake to clear the notice, if one is up.
    pub fn notice_deadline(&self) -> Option<std::time::Instant> {
        self.notice.as_ref().map(|notice| notice.until)
    }

    /// Drop an expired notice. Returns true when something changed, so the
    /// caller knows whether a redraw is owed.
    pub fn expire_notice(&mut self) -> bool {
        let expired = self
            .notice
            .as_ref()
            .is_some_and(|notice| std::time::Instant::now() >= notice.until);
        if expired {
            self.notice = None;
        }
        expired
    }

    // -----------------------------------------------------------------------
    // Transcript plumbing
    // -----------------------------------------------------------------------

    fn push_echo(&mut self, chat_id: &str, entry: SessionMessageEntry) {
        let echoes = self.echoes.entry(chat_id.to_string()).or_default();
        if !echoes.iter().any(|existing| existing.id == entry.id) {
            echoes.push(entry);
        }
        self.transcript_stale = true;
    }

    /// Remove an echo, returning its text so a failed send can be restored.
    fn drop_echo(&mut self, chat_id: &str, message_id: &str) -> Option<String> {
        let echoes = self.echoes.get_mut(chat_id)?;
        let index = echoes.iter().position(|echo| echo.id == message_id)?;
        let echo = echoes.remove(index);
        if echoes.is_empty() {
            self.echoes.remove(chat_id);
        }
        echo.parts.into_iter().find_map(|part| match part {
            MessagePart::Text { text, .. } => Some(text),
            _ => None,
        })
    }

    /// Re-lay out the transcript if anything changed. Called once per frame from
    /// the renderer with the real pane size, so a burst of doc frames between
    /// two draws costs one layout pass rather than one per frame — and a frame
    /// where nothing changed costs nothing at all.
    pub fn lay_out_transcript(&mut self, width: u16, height: usize) {
        self.viewport_height = height.max(1);
        if !self.transcript_stale && width == self.transcript_width {
            return;
        }
        self.transcript_width = width;
        self.transcript_stale = false;
        let echoes = self
            .selected_chat
            .as_deref()
            .and_then(|chat_id| self.echoes.get(chat_id))
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        self.transcript
            .set_entries(&self.doc_entries, echoes, width, &self.theme);
    }

    /// Rows of the transcript pane as of the last draw.
    pub fn viewport_height(&self) -> usize {
        self.viewport_height
    }

    // -----------------------------------------------------------------------
    // Overlays
    // -----------------------------------------------------------------------

    /// True while a floating panel owns the keyboard.
    pub fn overlay_open(&self) -> bool {
        self.help || self.overlay.is_some()
    }

    pub fn close_overlay(&mut self) {
        self.help = false;
        self.overlay = None;
    }

    /// The context menu for whatever the cursor is on — the same verbs the
    /// desktop app offers on right-click.
    pub fn open_context_menu(&mut self, column: u16, row: u16) {
        let target = self.rows.get(self.cursor).cloned();
        let (title, items) = match target {
            Some(Row::Chat {
                id,
                title,
                archived,
                orchestrator,
                ..
            }) => (
                title,
                vec![
                    MenuItem {
                        label: "Rename…".into(),
                        action: MenuAction::RenameChat(id.clone()),
                        separated: false,
                    },
                    MenuItem {
                        label: if archived { "Unarchive" } else { "Archive" }.into(),
                        action: MenuAction::SetArchived(id.clone(), !archived),
                        separated: false,
                    },
                    // Unpinning is the kill switch, so it is the same item
                    // rather than something to go and find: whoever pinned it
                    // reaches for the row they pinned.
                    MenuItem {
                        label: if orchestrator {
                            "Unpin as orchestrator"
                        } else {
                            "Pin as orchestrator"
                        }
                        .into(),
                        action: MenuAction::SetOrchestrator((!orchestrator).then(|| id.clone())),
                        separated: true,
                    },
                    MenuItem {
                        label: "Delete…".into(),
                        action: MenuAction::DeleteChat(id),
                        separated: true,
                    },
                ],
            ),
            Some(Row::Space { id, label, .. }) => (
                label,
                vec![
                    MenuItem {
                        label: "Rename space".into(),
                        action: MenuAction::RenameSpace(id.clone()),
                        separated: false,
                    },
                    MenuItem {
                        label: "Remove space…".into(),
                        action: MenuAction::DeleteSpace(id),
                        separated: true,
                    },
                ],
            ),
            _ => return,
        };
        self.overlay = Some(Overlay::Menu {
            title,
            items,
            active: 0,
            column,
            row,
        });
    }

    /// Open the branch picker. Only meaningful while drafting: an existing
    /// session's checkout is fixed.
    pub fn open_ref_picker(&mut self) -> Effects {
        let Some(draft) = &self.draft else {
            self.notify("Branch is chosen when a session is created (n).".into());
            return Vec::new();
        };
        let active = draft.picked_ref.unwrap_or_else(|| {
            draft
                .refs
                .as_ref()
                .and_then(|refs| refs.iter().position(|candidate| candidate.current))
                .unwrap_or(0)
        });
        self.overlay = Some(Overlay::Refs { active });
        Vec::new()
    }

    /// Open the checkout-kind picker (current checkout/worktree vs new).
    pub fn open_checkout_picker(&mut self) -> Effects {
        let Some(draft) = &self.draft else {
            self.notify("Where a session runs is chosen when it is created (n).".into());
            return Vec::new();
        };
        self.overlay = Some(Overlay::Checkout {
            active: match draft.checkout {
                CheckoutKind::Local => 0,
                CheckoutKind::NewWorktree => 1,
            },
        });
        Vec::new()
    }

    /// Open the model picker for the session's harness. Returns the query; the
    /// list fills in when `Update::Models` lands.
    pub fn open_model_picker(&mut self) -> Effects {
        // A draft has no chat row yet, but choosing its model is the whole
        // point of the canvas — so it is a valid target too.
        let harness = match (&self.draft, self.selected_chat_row()) {
            (Some(_), _) => comet_proto::HarnessId::ClaudeCode,
            (None, Some(chat)) => chat
                .config
                .as_ref()
                .map(|config| config.harness)
                .unwrap_or(comet_proto::HarnessId::ClaudeCode),
            (None, None) => {
                self.notify("Open a session first.".into());
                return Vec::new();
            }
        };
        self.overlay = Some(Overlay::Models {
            models: None,
            active: 0,
        });
        vec![Command::ListModels { harness }]
    }

    /// Move the selection inside whichever overlay is up.
    pub fn overlay_step(&mut self, delta: isize) {
        let (active, count) = match &mut self.overlay {
            Some(Overlay::Menu { active, items, .. }) => (active, items.len()),
            Some(Overlay::Refs { active }) => (
                active,
                self.draft
                    .as_ref()
                    .and_then(|draft| draft.refs.as_ref())
                    .map_or(0, Vec::len),
            ),
            Some(Overlay::Checkout { active }) => (active, 2),
            Some(Overlay::Reasoning { active, levels }) => (active, levels.len()),
            Some(Overlay::Models { active, models }) => {
                (active, models.as_ref().map_or(0, Vec::len))
            }
            // One longer than the logins: row 0 is the route's own account.
            Some(Overlay::DispatchAccount {
                active, accounts, ..
            }) => (active, accounts.as_ref().map_or(0, |list| list.len() + 1)),
            _ => return,
        };
        if count == 0 {
            return;
        }
        *active = (*active as isize + delta).rem_euclid(count as isize) as usize;
    }

    /// Activate the overlay's selection.
    pub fn overlay_confirm(&mut self) -> Effects {
        match self.overlay.take() {
            Some(Overlay::Menu { items, active, .. }) => match items.get(active) {
                Some(item) => self.run_menu_action(item.action.clone()),
                None => Vec::new(),
            },
            Some(Overlay::Refs { active }) => {
                if let Some(draft) = &mut self.draft {
                    draft.picked_ref = Some(active);
                    // Picking a ref that is already a worktree means "current
                    // worktree", which is Local — matching the desktop, where
                    // choosing a materialized ref flips the mode for you.
                    if draft
                        .refs
                        .as_ref()
                        .and_then(|refs| refs.get(active))
                        .is_some_and(|candidate| candidate.worktree_path.is_some())
                    {
                        draft.checkout = CheckoutKind::Local;
                    }
                }
                Vec::new()
            }
            Some(Overlay::Checkout { active }) => {
                if let Some(draft) = &mut self.draft {
                    draft.checkout = if active == 0 {
                        CheckoutKind::Local
                    } else {
                        CheckoutKind::NewWorktree
                    };
                }
                Vec::new()
            }
            Some(Overlay::Models { models, active }) => {
                let Some(model) = models.and_then(|list| list.get(active).cloned()) else {
                    return Vec::new();
                };
                if let Some(draft) = &mut self.draft {
                    // A draft has no chat to write to yet; it carries the choice
                    // to `createChat`.
                    draft.reasoning = draft
                        .reasoning
                        .filter(|level| model.reasoning_levels.contains(level))
                        .or_else(|| model.reasoning_levels.first().copied());
                    draft.model = Some(model);
                    return Vec::new();
                }
                self.set_model(model)
            }
            Some(Overlay::DispatchAccount {
                task_id,
                identifier,
                accounts,
                route_account,
                harness,
                active,
            }) => {
                // Still loading: enter would be a release nobody aimed. Put the
                // picker back, exactly as it was, and wait for the list.
                let Some(accounts) = accounts else {
                    self.overlay = Some(Overlay::DispatchAccount {
                        task_id,
                        identifier,
                        accounts: None,
                        route_account,
                        harness,
                        active,
                    });
                    return Vec::new();
                };
                // Row 0 sends no override — the route's account, which is what
                // every dispatch spent before this picker existed.
                let account = active
                    .checked_sub(1)
                    .and_then(|ix| accounts.get(ix))
                    .map(|a| a.id.clone());
                vec![self.dispatch_command(task_id, identifier, account)]
            }
            Some(Overlay::Reasoning { levels, active }) => {
                let Some(level) = levels.get(active).copied() else {
                    return Vec::new();
                };
                if let Some(draft) = &mut self.draft {
                    draft.reasoning = Some(level);
                    return Vec::new();
                }
                self.set_reasoning(level)
            }
            Some(Overlay::Prompt {
                input,
                action,
                title,
            }) => {
                let text = input.text().trim().to_string();
                if text.is_empty() {
                    // An empty rename is a no-op, not a wipe.
                    self.overlay = Some(Overlay::Prompt {
                        input,
                        action,
                        title,
                    });
                    return Vec::new();
                }
                match action {
                    PromptAction::RenameChat(chat_id) => vec![Command::Call {
                        method: methods::MUTATE,
                        params: serde_json::json!({
                            "op": "renameChat", "chatId": chat_id, "title": text,
                        }),
                        context: "Couldn't rename the session",
                    }],
                    PromptAction::RenameSpace(space_id) => vec![Command::Call {
                        method: methods::MUTATE,
                        params: serde_json::json!({
                            "op": "renameSpace", "spaceId": space_id, "name": text,
                        }),
                        context: "Couldn't rename the space",
                    }],
                }
            }
            None => Vec::new(),
        }
    }

    fn run_menu_action(&mut self, action: MenuAction) -> Effects {
        match action {
            MenuAction::RenameChat(chat_id) => {
                let current = self
                    .chats
                    .iter()
                    .find(|chat| chat.id == chat_id)
                    .and_then(|chat| chat.title.clone())
                    .unwrap_or_default();
                let mut input = Composer::default();
                input.set_text(current);
                self.overlay = Some(Overlay::Prompt {
                    title: "Rename session".into(),
                    input,
                    action: PromptAction::RenameChat(chat_id),
                });
                Vec::new()
            }
            MenuAction::RenameSpace(space_id) => {
                let current = self
                    .spaces
                    .iter()
                    .find(|space| space.id == space_id)
                    .map(|space| space.display_name().to_string())
                    .unwrap_or_default();
                let mut input = Composer::default();
                input.set_text(current);
                self.overlay = Some(Overlay::Prompt {
                    title: "Rename space".into(),
                    input,
                    action: PromptAction::RenameSpace(space_id),
                });
                Vec::new()
            }
            MenuAction::SetArchived(chat_id, archived) => vec![Command::Call {
                method: methods::MUTATE,
                params: serde_json::json!({
                    "op": "setChatArchived", "chatId": chat_id, "archived": archived,
                }),
                context: "Couldn't archive the session",
            }],
            MenuAction::SetOrchestrator(chat_id) => self.set_orchestrator(chat_id),
            MenuAction::DeleteChat(chat_id) => vec![Command::Call {
                method: methods::MUTATE,
                params: serde_json::json!({ "op": "deleteChat", "chatId": chat_id }),
                context: "Couldn't delete the session",
            }],
            MenuAction::DeleteSpace(space_id) => vec![Command::Call {
                method: methods::MUTATE,
                params: serde_json::json!({ "op": "deleteSpace", "spaceId": space_id }),
                context: "Couldn't remove the space",
            }],
            MenuAction::PickModel => self.open_model_picker(),
        }
    }

    /// Write an effort level onto the open session's config.
    fn set_reasoning(&mut self, level: comet_proto::ReasoningLevel) -> Effects {
        let Some(chat) = self.selected_chat_row() else {
            return Vec::new();
        };
        let chat_id = chat.id.clone();
        let mut config = chat.config.clone().unwrap_or(comet_proto::ChatConfig {
            harness: comet_proto::HarnessId::ClaudeCode,
            model: None,
            reasoning: None,
            model_options: Default::default(),
            sandbox: SandboxLevel::WorkspaceWrite,
            account: None,
            push_repo: None,
            git_author: None,
        });
        config.reasoning = Some(level);
        let Ok(value) = serde_json::to_value(&config) else {
            return Vec::new();
        };
        if let Some(row) = self.chats.iter_mut().find(|chat| chat.id == chat_id) {
            row.config = Some(config);
        }
        vec![Command::Call {
            method: methods::MUTATE,
            params: serde_json::json!({
                "op": "setChatConfig", "chatId": chat_id, "config": value,
            }),
            context: "Couldn't set effort",
        }]
    }

    /// Write a model onto the open session's config (a full-config LWW replace,
    /// as the desktop composer does).
    fn set_model(&mut self, model: comet_proto::Model) -> Effects {
        let Some(chat) = self.selected_chat_row() else {
            return Vec::new();
        };
        let chat_id = chat.id.clone();
        let mut config = chat.config.clone().unwrap_or(comet_proto::ChatConfig {
            harness: comet_proto::HarnessId::ClaudeCode,
            model: None,
            reasoning: None,
            model_options: Default::default(),
            sandbox: SandboxLevel::WorkspaceWrite,
            account: None,
            push_repo: None,
            git_author: None,
        });
        config.model = Some(model.id.clone());
        // Keep the reasoning level only if the new model offers it.
        if let Some(level) = config.reasoning
            && !model.reasoning_levels.contains(&level)
        {
            config.reasoning = model.reasoning_levels.first().copied();
        }
        let Ok(value) = serde_json::to_value(&config) else {
            return Vec::new();
        };
        // Echo locally so the chip updates on click; the chats frame confirms.
        if let Some(row) = self.chats.iter_mut().find(|chat| chat.id == chat_id) {
            row.config = Some(config);
        }
        vec![Command::Call {
            method: methods::MUTATE,
            params: serde_json::json!({
                "op": "setChatConfig", "chatId": chat_id, "config": value,
            }),
            context: "Couldn't switch model",
        }]
    }

    // -----------------------------------------------------------------------
    // Pointer
    // -----------------------------------------------------------------------

    /// Start a frame's hit map. Called once at the top of every draw.
    pub fn clear_hits(&mut self) {
        self.hits.clear();
    }

    /// Record that `hit` occupies `area`. Later calls win, so drawing order is
    /// z-order.
    pub fn push_hit(&mut self, area: ratatui::layout::Rect, hit: Hit) {
        self.hits.push((area, hit));
    }

    /// A click inside a floating panel's rows. `None` when it landed elsewhere,
    /// which the caller treats as a dismissal.
    fn click_overlay(&mut self, _column: u16, row: u16) -> Option<Effects> {
        let (top, count) = match &self.overlay {
            Some(Overlay::Menu {
                row: anchor, items, ..
            }) => (anchor.saturating_add(2), items.len()),
            // The picker is centred, so its rows are not addressable from the
            // anchor; keyboard drives it and a click just dismisses.
            _ => return None,
        };
        let index = row.checked_sub(top)? as usize;
        if index >= count {
            return None;
        }
        if let Some(Overlay::Menu { active, .. }) = &mut self.overlay {
            *active = index;
        }
        Some(self.overlay_confirm())
    }

    /// What is under the pointer, if anything.
    pub fn hit_test(&self, column: u16, row: u16) -> Option<Hit> {
        self.hits
            .iter()
            .rev()
            .find(|(area, _)| {
                column >= area.x
                    && column < area.x.saturating_add(area.width)
                    && row >= area.y
                    && row < area.y.saturating_add(area.height)
            })
            .map(|(_, hit)| hit.clone())
    }

    /// Apply a right click: open the context menu for whatever is under it.
    pub fn right_click(&mut self, column: u16, row: u16) {
        if self.overlay_open() {
            self.close_overlay();
            return;
        }
        // Right-clicking a row targets *that* row, not wherever the cursor
        // happened to be — the same as clicking it first.
        match self.hit_test(column, row) {
            Some(Hit::Row(index)) => {
                self.cursor = index.min(self.rows.len().saturating_sub(1));
                self.open_context_menu(column, row);
            }
            Some(Hit::Tab(index)) => {
                if let Some(tab) = self.tabs().get(index)
                    && let Some(at) = self
                        .rows
                        .iter()
                        .position(|candidate| candidate.id() == Some(tab.id.as_str()))
                {
                    self.cursor = at;
                    self.open_context_menu(column, row);
                }
            }
            _ => {}
        }
    }

    /// Apply a left click at a cell.
    pub fn click(&mut self, column: u16, row: u16) -> Effects {
        // A click while a panel is up dismisses it, wherever it lands — the
        // panel covers the body, so anything under it is unreachable.
        if self.overlay.is_some() {
            // Inside the panel, a click selects the row it landed on.
            if let Some(effects) = self.click_overlay(column, row) {
                return effects;
            }
            self.overlay = None;
            return Vec::new();
        }
        if self.help {
            self.help = false;
            return Vec::new();
        }
        match self.hit_test(column, row) {
            Some(Hit::Row(index)) => {
                self.cursor = index.min(self.rows.len().saturating_sub(1));
                self.focus = Focus::Sidebar;
                self.open_row()
            }
            Some(Hit::Tab(index)) => self.select_tab(index + 1),
            Some(Hit::NewSession) => self.new_session(),
            Some(Hit::Chip(ChipKind::Branch)) => self.open_ref_picker(),
            Some(Hit::Chip(ChipKind::Checkout)) => self.open_checkout_picker(),
            Some(Hit::Chip(ChipKind::Model)) => self.open_model_picker(),
            Some(Hit::Chip(ChipKind::Reasoning)) => self.open_reasoning_picker(),
            Some(Hit::BoardRow(index)) => {
                // Clicking a board row selects it; enter dispatches. Like the
                // sidebar, the row is the target, not wherever the cursor was.
                if let Some(line) = self.board.lines().get(index).cloned() {
                    self.board.selected = Some(line.id());
                    self.focus = Focus::Board;
                }
                Vec::new()
            }
            Some(Hit::AddSpace) => {
                self.notify(
                    "Adding a space needs a folder picker — use the desktop app for now.".into(),
                );
                Vec::new()
            }
            Some(Hit::Pane(focus)) => {
                self.focus = focus;
                Vec::new()
            }
            Some(Hit::Overlay) => {
                self.help = false;
                Vec::new()
            }
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use comet_proto::{SessionStatus, view::ConnectionStatus};

    /// Long enough that it wraps differently at 60 and at 30 columns, which is
    /// what the resize-invalidation test actually needs to observe.
    const LONG_LINE: &str =
        "a line of output long enough that its wrap points move when the pane narrows";

    fn space(id: &str, path: &str, created_min: i64) -> Space {
        Space {
            id: id.into(),
            device_id: "dev".into(),
            path: path.into(),
            name: None,
            git_detected: true,
            git_checked_at: None,
            checkout_id: None,
            created_at: Utc::now() + chrono::Duration::minutes(created_min),
        }
    }

    fn chat(id: &str, space_id: &str, created_min: i64) -> Chat {
        Chat {
            id: id.into(),
            device_id: "dev".into(),
            title: Some(format!("chat {id}")),
            archived: false,
            cwd: None,
            branch: None,
            checkout_id: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: Utc::now() + chrono::Duration::minutes(created_min),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: Some(space_id.into()),
            last_seen_at: None,
        }
    }

    fn session(chat_id: &str, status: SessionStatus, age_secs: i64) -> Session {
        Session {
            chat_id: chat_id.into(),
            device_id: "dev".into(),
            status,
            started_at: None,
            updated_at: Utc::now() - chrono::Duration::seconds(age_secs),
        }
    }

    /// An app with one space and two sessions, nothing selected yet.
    fn seeded() -> App {
        let mut app = App::with_theme(crate::theme::Theme::dark());
        app.apply(Update::Spaces(vec![space("s1", "/dev/comet", 0)]));
        app.apply(Update::Chats(vec![
            chat("c1", "s1", 1),
            chat("c2", "s1", 2),
        ]));
        app
    }

    fn is_watch(command: &Command, expected: Option<&str>) -> bool {
        matches!(command, Command::WatchTranscript(target)
            if target.as_deref() == expected)
    }

    fn mutate_op(command: &Command) -> Option<&str> {
        match command {
            Command::Call { params, .. } => params.get("op")?.as_str(),
            _ => None,
        }
    }

    #[test]
    fn first_chats_frame_auto_selects_and_subscribes() {
        let app = seeded();
        // `chats` is recency sorted; the first non-archived row is opened so the
        // pane is never blank when there is something to show.
        assert!(app.selected_chat.is_some());
        assert_eq!(app.selected_space.as_deref(), Some("s1"));
        // The cursor lands on the row that was opened, not on row 0 (the space
        // header) — otherwise `n` would target the wrong thing.
        assert_eq!(app.rows[app.cursor].id(), app.selected_chat.as_deref());
    }

    #[test]
    fn selecting_a_chat_swaps_the_subscription_and_marks_it_read() {
        let mut app = seeded();
        let mut unseen = chat("c3", "s1", 3);
        unseen.last_message_at = Some(Utc::now());
        unseen.last_seen_at = None; // unseen
        app.apply(Update::Chats(vec![
            chat("c1", "s1", 1),
            chat("c2", "s1", 2),
            unseen,
        ]));

        let effects = app.select_chat(Some("c3".into()));
        assert!(
            effects.iter().any(|c| is_watch(c, Some("c3"))),
            "the visible chat's doc must be the only one streamed"
        );
        assert!(
            effects.iter().any(|c| mutate_op(c) == Some("markChatSeen")),
            "opening an unseen session clears the badge everywhere"
        );

        // A chat with nothing unseen doesn't spend a mutation.
        let effects = app.select_chat(Some("c1".into()));
        assert!(effects.iter().any(|c| is_watch(c, Some("c1"))));
        assert!(!effects.iter().any(|c| mutate_op(c) == Some("markChatSeen")));

        // Re-selecting the same chat is a no-op: no resubscribe, no flicker.
        assert!(app.select_chat(Some("c1".into())).is_empty());
    }

    #[test]
    fn a_vanished_chat_drops_the_selection_and_unsubscribes() {
        let mut app = seeded();
        app.select_chat(Some("c2".into()));
        // c2 deleted on another device.
        let effects = app.apply(Update::Chats(vec![chat("c1", "s1", 1)]));
        assert_eq!(app.selected_chat, None);
        assert!(
            effects.iter().any(|c| is_watch(c, None)),
            "the dead chat's stream must be cancelled"
        );
    }

    #[test]
    fn send_needs_a_session_and_never_eats_the_prompt() {
        let mut app = App::with_theme(crate::theme::Theme::dark());
        app.composer.set_text("do the thing");
        let effects = app.act(Action::Send);
        assert!(effects.is_empty());
        assert_eq!(
            app.composer.text(),
            "do the thing",
            "a send with nowhere to go must not discard what was typed"
        );
        assert!(app.notice.is_some());
    }

    #[test]
    fn send_queues_a_run_with_the_spaces_cwd_and_echoes_immediately() {
        let mut app = seeded();
        app.select_chat(Some("c1".into()));
        app.composer.set_text("ship it");
        let effects = app.act(Action::Send);

        let Some(Command::Send {
            chat_id,
            params,
            message_id,
        }) = effects.iter().find(|c| matches!(c, Command::Send { .. }))
        else {
            panic!("expected a Send, got {effects:?}");
        };
        assert_eq!(chat_id, "c1");
        let command = &params["command"];
        assert_eq!(command["kind"], "run", "an idle session starts a run");
        assert_eq!(command["request"]["prompt"], "ship it");
        // cwd falls back to the space's folder when the chat has none of its own.
        assert_eq!(command["request"]["cwd"], "/dev/comet");
        assert_eq!(command["messageId"], message_id.as_str());

        // The composer is cleared and the text now lives in the echo, so it is
        // visible before the doc round-trips.
        assert!(app.composer.is_empty());
        app.lay_out_transcript(60, 10);
        assert!(!app.transcript.is_empty(), "the echo must render at once");
        assert!(app.transcript.following());
    }

    #[test]
    fn send_steers_a_running_session_instead_of_starting_a_second_run() {
        let mut app = seeded();
        app.select_chat(Some("c1".into()));
        app.apply(Update::Sessions(vec![session(
            "c1",
            SessionStatus::Working,
            1,
        )]));
        app.composer.set_text("also check the tests");
        let effects = app.act(Action::Send);
        let Some(Command::Send { params, .. }) =
            effects.iter().find(|c| matches!(c, Command::Send { .. }))
        else {
            panic!("expected a Send");
        };
        assert_eq!(params["command"]["kind"], "steer");
        assert_eq!(params["command"]["prompt"], "also check the tests");
    }

    #[test]
    fn a_stale_working_session_starts_a_run_not_a_steer() {
        // A crashed engine leaves a Working row behind; steering into a run that
        // no longer exists would silently drop the prompt.
        let mut app = seeded();
        app.select_chat(Some("c1".into()));
        app.apply(Update::Sessions(vec![session(
            "c1",
            SessionStatus::Working,
            120,
        )]));
        app.composer.set_text("hello");
        let effects = app.act(Action::Send);
        let Some(Command::Send { params, .. }) =
            effects.iter().find(|c| matches!(c, Command::Send { .. }))
        else {
            panic!("expected a Send");
        };
        assert_eq!(params["command"]["kind"], "run");
    }

    #[test]
    fn a_failed_send_hands_the_text_back() {
        let mut app = seeded();
        app.select_chat(Some("c1".into()));
        app.composer.set_text("expensive prompt");
        let effects = app.act(Action::Send);
        let Some(Command::Send { message_id, .. }) = effects
            .into_iter()
            .find(|c| matches!(c, Command::Send { .. }))
        else {
            panic!("expected a Send");
        };
        assert!(app.composer.is_empty());

        app.apply(Update::SendFailed {
            chat_id: "c1".into(),
            message_id,
            error: "engine unreachable".into(),
        });
        assert_eq!(app.composer.text(), "expensive prompt");
        // The echo is gone, so the transcript doesn't show a message that was
        // never sent.
        app.lay_out_transcript(60, 10);
        assert!(app.transcript.is_empty());
        assert!(app.notice.is_some());
    }

    #[test]
    fn a_doc_frame_supersedes_its_echo_without_duplicating_it() {
        let mut app = seeded();
        app.select_chat(Some("c1".into()));
        app.composer.set_text("hi");
        let effects = app.act(Action::Send);
        let Some(Command::Send { message_id, .. }) = effects
            .into_iter()
            .find(|c| matches!(c, Command::Send { .. }))
        else {
            panic!("expected a Send");
        };
        app.lay_out_transcript(60, 10);
        let with_echo = app.transcript.total_lines();

        // The doc confirms the same client-minted id.
        app.apply(Update::Transcript {
            chat_id: "c1".into(),
            entries: vec![SessionMessageEntry {
                id: message_id,
                role: MessageRole::User,
                parts: vec![MessagePart::Text {
                    id: "t0".into(),
                    text: "hi".into(),
                }],
                created_at: Utc::now().timestamp_millis(),
                device_id: "dev".into(),
                status: None,
                continuation_of: None,
            }],
        });
        app.lay_out_transcript(60, 10);
        assert_eq!(
            app.transcript.total_lines(),
            with_echo,
            "the confirmed message must replace the echo, not stack on it"
        );
    }

    #[test]
    fn transcript_frames_for_other_chats_are_ignored() {
        let mut app = seeded();
        app.select_chat(Some("c1".into()));
        app.apply(Update::Transcript {
            chat_id: "c2".into(),
            entries: vec![SessionMessageEntry {
                id: "m".into(),
                role: MessageRole::Assistant,
                parts: vec![MessagePart::Text {
                    id: "t0".into(),
                    text: "from the wrong chat".into(),
                }],
                created_at: 0,
                device_id: "dev".into(),
                status: None,
                continuation_of: None,
            }],
        });
        app.lay_out_transcript(60, 10);
        assert!(
            app.transcript.is_empty(),
            "a frame that raced a selection change must not render"
        );
    }

    #[test]
    fn rows_keep_the_cursor_on_its_row_when_the_list_shifts() {
        let mut app = seeded();
        // Park on c2.
        let target = app.rows.iter().position(|r| r.id() == Some("c2")).unwrap();
        app.cursor = target;

        // A new chat sorts above it (chats are recency-ordered, and rows within
        // a space are creation-ordered — either way the index moves).
        app.apply(Update::Chats(vec![
            chat("c0", "s1", -5),
            chat("c1", "s1", 1),
            chat("c2", "s1", 2),
        ]));
        assert_eq!(
            app.rows[app.cursor].id(),
            Some("c2"),
            "the cursor must follow its row, not its index"
        );
    }

    #[test]
    fn archived_rows_are_hidden_until_asked_for() {
        let mut app = seeded();
        let mut archived = chat("c2", "s1", 2);
        archived.archived = true;
        app.apply(Update::Chats(vec![chat("c1", "s1", 1), archived]));
        assert!(!app.rows.iter().any(|row| row.id() == Some("c2")));

        // Otherwise an archived session could never be unarchived.
        app.act(Action::ToggleShowArchived);
        assert!(app.rows.iter().any(|row| row.id() == Some("c2")));

        // And archiving is a mutation on the row under the cursor.
        app.cursor = app.rows.iter().position(|r| r.id() == Some("c1")).unwrap();
        let effects = app.act(Action::ToggleArchive);
        assert_eq!(mutate_op(&effects[0]), Some("setChatArchived"));
        match &effects[0] {
            Command::Call { params, .. } => {
                assert_eq!(params["chatId"], "c1");
                assert_eq!(params["archived"], true);
            }
            other => panic!("expected a Call, got {other:?}"),
        }
    }

    #[test]
    fn a_new_session_is_a_draft_until_the_first_send() {
        // The desktop app opens a canvas and materializes the tab on send;
        // abandoning it must leave nothing behind.
        let mut app = seeded();
        app.apply(Update::Connection(ConnectionStatus::Ready));
        let before = app.chats.len();
        let effects = app.act(Action::NewSession);

        assert!(app.draft.is_some(), "a draft, not a chat");
        assert_eq!(app.selected_chat, None, "the draft owns the pane");
        assert_eq!(app.chats.len(), before, "nothing was created");
        assert!(
            !effects.iter().any(|c| mutate_op(c) == Some("createChat")),
            "createChat must wait for the first send"
        );
        // It shows as a pending tab so the pane is not simply blank.
        assert!(app.tabs().iter().any(|tab| tab.active && tab.id.is_empty()));

        // Esc abandons it cleanly.
        app.act(Action::CloseOverlay);
        assert!(app.draft.is_none());
        assert_eq!(app.chats.len(), before);
    }

    #[test]
    fn sending_a_draft_materializes_it_in_one_ordered_command() {
        let mut app = seeded();
        app.apply(Update::Connection(ConnectionStatus::Ready));
        app.act(Action::NewSession);
        app.composer.set_text("first prompt");
        let effects = app.act(Action::Send);

        let start = effects
            .iter()
            .find_map(|c| match c {
                Command::StartSession(start) => Some(start),
                _ => None,
            })
            .expect("a StartSession");
        assert_eq!(start.space_id, "s1");
        assert_eq!(start.command["kind"], "run");
        assert_eq!(start.command["request"]["prompt"], "first prompt");
        // Default plan with no refs loaded: the space folder as-is.
        assert_eq!(start.plan, CheckoutPlan::CurrentCheckout);

        // The draft is gone and its echo is already on screen.
        assert!(app.draft.is_none());
        assert_eq!(app.selected_chat.as_deref(), Some(start.chat_id.as_str()));
        app.lay_out_transcript(60, 10);
        assert!(!app.transcript.is_empty(), "the echo renders at once");
    }

    #[test]
    fn the_checkout_choice_drives_the_plan() {
        use comet_proto::RepoRef;
        let mut app = seeded();
        app.apply(Update::Connection(ConnectionStatus::Ready));
        app.act(Action::NewSession);
        app.apply(Update::Refs(vec![
            RepoRef {
                name: "main".into(),
                current: true,
                worktree_path: None,
            },
            RepoRef {
                name: "feat".into(),
                current: false,
                worktree_path: Some("/wt/feat".into()),
            },
        ]));

        // Default: current branch, no worktree -> run in place.
        let draft = app.draft.as_ref().unwrap();
        assert_eq!(draft.ref_label(), "main");
        assert_eq!(draft.checkout_label(), "Current checkout");
        assert_eq!(draft.plan(), CheckoutPlan::CurrentCheckout);

        // Switch to a new worktree off main.
        app.act(Action::PickCheckout);
        app.act(Action::OverlayStep(1));
        app.act(Action::OverlayConfirm);
        let draft = app.draft.as_ref().unwrap();
        assert_eq!(draft.checkout_label(), "New worktree");
        assert_eq!(
            draft.plan(),
            CheckoutPlan::NewWorktree {
                base: Some("main".into())
            }
        );

        // Picking a ref that is already materialized flips back to Local and
        // resolves as "current worktree" — the third outcome.
        app.act(Action::PickRef);
        app.act(Action::OverlayStep(1));
        app.act(Action::OverlayConfirm);
        let draft = app.draft.as_ref().unwrap();
        assert_eq!(draft.checkout_label(), "Current worktree");
        assert_eq!(
            draft.plan(),
            CheckoutPlan::ReuseWorktree {
                path: "/wt/feat".into(),
                branch: "feat".into()
            }
        );
        // And the footer says so, since the choice is only offered before send.
        let footer = app.composer_footer().expect("a git space has a footer");
        assert_eq!(footer.reference, "feat");
        assert_eq!(footer.checkout, "Current worktree");
        assert!(footer.checkout_live, "a draft can still move");
    }

    #[test]
    fn new_session_targets_the_space_under_the_cursor() {
        let mut app = App::with_theme(crate::theme::Theme::dark());
        app.apply(Update::Spaces(vec![
            space("s1", "/dev/a", 0),
            space("s2", "/dev/b", 1),
        ]));
        app.cursor = app.rows.iter().position(|r| r.id() == Some("s2")).unwrap();
        app.act(Action::NewSession);
        assert_eq!(
            app.draft.as_ref().map(|draft| draft.space_id.as_str()),
            Some("s2"),
            "the draft targets the space under the cursor"
        );
        // The prompt is focused, so you can type straight away.
        assert_eq!(app.focus, Focus::Composer);
    }

    #[test]
    fn a_watch_frame_racing_create_does_not_steal_the_new_session() {
        // The window this closes: `n` selects the new chat optimistically, and a
        // chats frame arriving before the createChat mutation lands would once
        // have healed the selection back to the previous session — dropping the
        // user (and anything they had started typing) onto the wrong chat.
        let mut app = seeded();
        app.apply(Update::Connection(ConnectionStatus::Ready));
        app.cursor = app.rows.iter().position(|r| r.id() == Some("s1")).unwrap();
        app.act(Action::NewSession);
        app.composer.set_text("go");
        app.act(Action::Send);
        let created = app.selected_chat.clone().expect("selected after send");

        // A frame that predates the new row.
        let effects = app.apply(Update::Chats(vec![
            chat("c1", "s1", 1),
            chat("c2", "s1", 2),
        ]));
        assert_eq!(
            app.selected_chat.as_deref(),
            Some(created.as_str()),
            "the pending session must survive a racing frame"
        );
        assert!(
            !effects.iter().any(|c| is_watch(c, None)),
            "and its transcript must not be unsubscribed"
        );

        // Once the row lands, normal healing resumes.
        let mut landed = chat(&created, "s1", 3);
        landed.title = None;
        app.apply(Update::Chats(vec![chat("c1", "s1", 1), landed]));
        assert_eq!(app.selected_chat.as_deref(), Some(created.as_str()));
        assert!(
            app.rows
                .iter()
                .any(|row| row.id() == Some(created.as_str()))
        );

        // A createChat that never lands must not wedge the selection forever.
        app.act(Action::NewSession);
        app.composer.set_text("go");
        app.act(Action::Send);
        let doomed = app.selected_chat.clone().unwrap();
        app.pending_chat.as_mut().unwrap().1 =
            std::time::Instant::now() - PENDING_CHAT_GRACE - std::time::Duration::from_secs(1);
        app.apply(Update::Chats(vec![chat("c1", "s1", 1)]));
        assert_ne!(
            app.selected_chat.as_deref(),
            Some(doomed.as_str()),
            "after the grace period a failed create heals like any dead chat"
        );
    }

    #[test]
    fn new_session_without_a_space_explains_itself() {
        let mut app = App::with_theme(crate::theme::Theme::dark());
        assert!(app.act(Action::NewSession).is_empty());
        assert!(app.notice.is_some());
    }

    #[test]
    fn interrupt_only_fires_with_a_session_open() {
        let mut app = seeded();
        app.select_chat(None);
        assert!(app.act(Action::Interrupt).is_empty());

        app.select_chat(Some("c1".into()));
        let effects = app.act(Action::Interrupt);
        match &effects[0] {
            Command::Call { params, .. } => {
                assert_eq!(params["chatId"], "c1");
                assert_eq!(params["command"]["kind"], "interrupt");
            }
            other => panic!("expected an interrupt, got {other:?}"),
        }
    }

    #[test]
    fn hiding_the_sidebar_moves_focus_off_it() {
        let mut app = seeded();
        app.focus = Focus::Sidebar;
        app.act(Action::ToggleSidebar);
        assert!(!app.sidebar_visible);
        assert_ne!(app.focus, Focus::Sidebar);
        // And Tab skips the hidden pane instead of focusing nothing.
        app.focus = Focus::Composer;
        app.act(Action::FocusNext);
        assert_eq!(app.focus, Focus::Transcript);
    }

    #[test]
    fn only_a_live_session_or_the_boot_gate_animates() {
        let mut app = seeded();
        // The boot gate's comet mark animates while connecting…
        assert!(app.animating(), "the boot splash animates");
        app.apply(Update::Connection(ConnectionStatus::Ready));
        // …but once connected and idle, nothing does, so the loop arms no timer.
        assert!(!app.animating(), "an idle app must arm no timer at all");
        app.apply(Update::Sessions(vec![session(
            "c1",
            SessionStatus::Working,
            1,
        )]));
        assert!(app.animating());
        // Staleness gating: a crashed engine's leftover row stops the spinner
        // rather than animating forever.
        app.apply(Update::Sessions(vec![session(
            "c1",
            SessionStatus::Working,
            120,
        )]));
        assert!(!app.animating());
        // Waiting on input is not motion.
        app.apply(Update::Sessions(vec![session(
            "c1",
            SessionStatus::AwaitingInput,
            1,
        )]));
        assert!(!app.animating());
    }

    #[test]
    fn the_space_row_aggregates_its_sessions() {
        let mut app = seeded();
        app.apply(Update::Sessions(vec![
            session("c1", SessionStatus::Working, 1),
            session("c2", SessionStatus::AwaitingInput, 1),
        ]));
        // The Spaces section carries the aggregate dot; the Sessions list below
        // is flat and global.
        let Some(Row::Space {
            label, attention, ..
        }) = app.rows.iter().find(|row| row.id() == Some("s1"))
        else {
            panic!("expected the space row");
        };
        assert_eq!(label, "comet");
        assert_eq!(
            *attention,
            Some(ChatIndicator::AwaitingInput),
            "the space row surfaces its most urgent session"
        );
        let statuses: Vec<ChatIndicator> = app
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Chat { indicator, .. } => Some(*indicator),
                _ => None,
            })
            .collect();
        assert!(statuses.contains(&ChatIndicator::Working), "{statuses:?}");
        assert!(
            statuses.contains(&ChatIndicator::AwaitingInput),
            "{statuses:?}"
        );
    }

    #[test]
    fn gate_follows_connection_then_auth() {
        let mut app = App::with_theme(crate::theme::Theme::dark());
        assert_eq!(app.gate(), GatePhase::Loading);
        app.apply(Update::Connection(ConnectionStatus::Failed("boom".into())));
        assert_eq!(app.gate(), GatePhase::Failed("boom".into()));
        app.apply(Update::Connection(ConnectionStatus::Ready));
        // No auth frame yet (dev-mode engine) gates nothing.
        assert_eq!(app.gate(), GatePhase::Ready);
        app.apply(Update::Auth(Box::new(AuthState::SignedOut)));
        assert_eq!(app.gate(), GatePhase::SignIn);
    }

    #[test]
    fn notices_expire_on_their_own_deadline() {
        let mut app = App::with_theme(crate::theme::Theme::dark());
        assert_eq!(app.notice_deadline(), None, "no notice arms no wakeup");
        app.notify("something happened".into());
        assert!(app.notice_deadline().is_some());
        assert!(!app.expire_notice(), "not yet");
        // Reach into the deadline rather than sleeping six seconds.
        app.notice.as_mut().unwrap().until = std::time::Instant::now();
        assert!(app.expire_notice());
        assert!(app.notice.is_none());
        assert_eq!(app.notice_deadline(), None);
    }

    #[test]
    fn layout_is_skipped_when_nothing_changed() {
        let mut app = seeded();
        app.select_chat(Some("c1".into()));
        app.apply(Update::Transcript {
            chat_id: "c1".into(),
            entries: vec![SessionMessageEntry {
                id: "m".into(),
                role: MessageRole::Assistant,
                parts: vec![MessagePart::Text {
                    id: "t0".into(),
                    text: LONG_LINE.into(),
                }],
                created_at: 0,
                device_id: "dev".into(),
                status: None,
                continuation_of: None,
            }],
        });
        app.lay_out_transcript(60, 10);
        let lines = app.transcript.total_lines();
        assert!(lines > 0);

        // A second draw at the same width with no new frames must be a no-op —
        // this is what makes an idle-but-focused app free.
        assert!(!app.transcript_stale);
        app.lay_out_transcript(60, 10);
        assert_eq!(app.transcript.total_lines(), lines);

        // A resize does re-lay out.
        app.lay_out_transcript(30, 10);
        assert_ne!(app.transcript.total_lines(), lines);
    }

    #[test]
    fn scroll_actions_use_the_last_drawn_viewport() {
        let mut app = seeded();
        app.select_chat(Some("c1".into()));
        app.apply(Update::Transcript {
            chat_id: "c1".into(),
            entries: (0..40)
                .map(|i| SessionMessageEntry {
                    id: format!("m{i}"),
                    role: MessageRole::Assistant,
                    parts: vec![MessagePart::Text {
                        id: "t0".into(),
                        text: "line".into(),
                    }],
                    created_at: 0,
                    device_id: "dev".into(),
                    status: None,
                    continuation_of: None,
                })
                .collect(),
        });
        app.lay_out_transcript(60, 10);
        assert_eq!(app.viewport_height(), 10);
        assert!(app.transcript.following());

        app.act(Action::PageUp);
        assert!(!app.transcript.following());
        app.act(Action::ScrollBottom);
        assert!(app.transcript.following());
    }

    // -----------------------------------------------------------------------
    // Board
    // -----------------------------------------------------------------------

    fn board_row(id: &str, state: comet_proto::view::board::BoardState) -> comet_proto::view::board::TaskRow {
        use comet_proto::view::board::TaskRow;
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
            updated_at: chrono::Utc::now().to_rfc3339(),
            started_at: None,
            account: None,
            dispatched_by_user: None,
            billed_to: None,
            max_duration_secs: None,
        }
    }

    #[test]
    fn b_toggles_the_board_and_owning_it_focuses_it() {
        let mut app = seeded();
        assert!(!app.board_open);
        app.act(Action::ToggleBoard);
        assert!(app.board_open);
        assert_eq!(app.focus, Focus::Board);
        app.act(Action::ToggleBoard);
        assert!(!app.board_open);
        assert_eq!(app.focus, Focus::Sidebar, "back to the chat panes");
    }

    #[test]
    fn the_board_stream_seeds_rows_and_lands_the_cursor_on_a_task() {
        let mut app = seeded();
        app.act(Action::ToggleBoard);
        app.apply(Update::Board(vec![
            board_row("1", BoardState::Ready),
            board_row("2", BoardState::Working),
            board_row("3", BoardState::Done),
            board_row("4", BoardState::Blocked),
        ]));
        // Sections in fixed order, done folded by default; the cursor starts on
        // the first *task*, which is the blocked row (fixed order, not arrival).
        assert!(app.board.selected_task().is_some());
        assert_eq!(
            app.board
                .lines()
                .into_iter()
                .filter(|line| matches!(line, crate::board::BoardRow::Task(_)))
                .count(),
            3,
            "done starts folded, so its one row is hidden"
        );
        assert_eq!(
            app.board.selected.as_deref(),
            Some("4"),
            "blocked is the first section, so it owns the cursor"
        );
    }

    #[test]
    fn enter_asks_which_account_for_a_ready_row_and_not_an_unrouted_one() {
        let mut app = seeded();
        app.act(Action::ToggleBoard);
        let mut ready = board_row("1", BoardState::Ready);
        ready.route = Some("offhand".into());
        ready.runtime = Some("claude-code".into());
        let mut unrouted = board_row("2", BoardState::Ready);
        unrouted.dispatchable = false;
        app.apply(Update::Board(vec![ready, unrouted]));

        // Cursor starts on "1" (first task, ready section).
        app.board.selected = Some("1".into());
        let effects = app.act(Action::BoardEnter);
        match effects.first() {
            Some(Command::ListDispatchAccounts { task_id, runtime }) => {
                assert_eq!(task_id, "1");
                assert_eq!(runtime.as_deref(), Some("claude-code"));
            }
            other => panic!("expected an account fetch, got {other:?}"),
        }
        assert!(
            matches!(app.overlay, Some(Overlay::DispatchAccount { .. })),
            "the picker is up while the logins load"
        );

        // A row nothing routes dispatches nowhere and says why.
        app.overlay = None;
        app.board.selected = Some("2".into());
        assert!(app.act(Action::BoardEnter).is_empty());
        assert!(
            app.notice.as_ref().is_some_and(|n| n.text.contains("no route")),
            "the reason must be said"
        );
    }

    fn agent_account(id: &str, email: &str) -> comet_proto::AgentAccount {
        comet_proto::AgentAccount {
            id: id.into(),
            harness: comet_proto::HarnessId::ClaudeCode,
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

    /// gh#74: the picker's first row is the route's account (no override), and
    /// every row sends who released it.
    #[test]
    fn the_account_picker_releases_on_the_route_or_on_the_picked_slot() {
        let mut app = seeded();
        app.local_device_id = Some("laptop".into());
        app.act(Action::ToggleBoard);
        let mut ready = board_row("1", BoardState::Ready);
        ready.route = Some("offhand".into());
        app.apply(Update::Board(vec![ready]));
        app.board.selected = Some("1".into());

        // Row 0: no account override — what enter alone always did.
        app.act(Action::BoardEnter);
        app.apply(Update::DispatchAccounts {
            harness: Some(comet_proto::HarnessId::ClaudeCode),
            task_id: "1".into(),
            accounts: vec![agent_account("slot-ana", "ana@example.com")],
        });
        match app.act(Action::OverlayConfirm).first() {
            Some(Command::Dispatch {
                task_id,
                account,
                via_device,
                ..
            }) => {
                assert_eq!(task_id, "1");
                assert_eq!(*account, None, "the route's account needs no override");
                assert_eq!(via_device.as_deref(), Some("laptop"));
            }
            other => panic!("expected a dispatch, got {other:?}"),
        }

        // Row 1: that slot, sent as the override.
        app.act(Action::BoardEnter);
        app.apply(Update::DispatchAccounts {
            harness: Some(comet_proto::HarnessId::ClaudeCode),
            task_id: "1".into(),
            accounts: vec![agent_account("slot-ana", "ana@example.com")],
        });
        app.act(Action::OverlayStep(1));
        match app.act(Action::OverlayConfirm).first() {
            Some(Command::Dispatch { account, .. }) => {
                assert_eq!(account.as_deref(), Some("slot-ana"));
            }
            other => panic!("expected a dispatch, got {other:?}"),
        }
    }

    /// A reply for a row the operator has moved off must not fill the picker
    /// that replaced it.
    #[test]
    fn a_stale_account_reply_is_dropped() {
        let mut app = seeded();
        app.act(Action::ToggleBoard);
        let mut ready = board_row("1", BoardState::Ready);
        ready.route = Some("offhand".into());
        app.apply(Update::Board(vec![ready]));
        app.board.selected = Some("1".into());
        app.act(Action::BoardEnter);
        app.apply(Update::DispatchAccounts {
            harness: Some(comet_proto::HarnessId::ClaudeCode),
            task_id: "9".into(),
            accounts: vec![agent_account("slot-ana", "ana@example.com")],
        });
        assert!(
            matches!(
                app.overlay,
                Some(Overlay::DispatchAccount { accounts: None, .. })
            ),
            "another row's logins are not this row's"
        );
        // And enter while nothing has landed releases nothing.
        assert!(app.act(Action::OverlayConfirm).is_empty());
        assert!(matches!(app.overlay, Some(Overlay::DispatchAccount { .. })));
    }

    // ---- the pinned orchestrator (gh#104) --------------------------------

    /// Every other row's position is recency; the orchestrator's position *is*
    /// the designation, so it does not slide down the list the moment two other
    /// sessions say something.
    #[test]
    fn the_pinned_orchestrator_sits_at_the_top_of_the_sessions_list() {
        let mut app = seeded();
        let first_chat = |app: &App| {
            app.rows
                .iter()
                .find_map(|row| match row {
                    Row::Chat {
                        id, orchestrator, ..
                    } => Some((id.clone(), *orchestrator)),
                    _ => None,
                })
                .expect("a chat row")
        };
        // c2 is the more recent, so it leads before anything is pinned.
        assert_eq!(first_chat(&app), ("c2".into(), false));

        app.apply(Update::Orchestrator(Some("c1".into())));
        assert_eq!(first_chat(&app), ("c1".into(), true));

        // And unpinning hands the list straight back to recency.
        app.apply(Update::Orchestrator(None));
        assert_eq!(first_chat(&app), ("c2".into(), false));
    }

    /// A pin the board does not know about — an archived chat, a stale id left
    /// in `routing.toml` — must not reorder or mark anything. Nothing is
    /// synthesised into the list to stand in for it.
    #[test]
    fn a_pin_naming_no_visible_chat_changes_nothing() {
        let mut app = seeded();
        let before: Vec<String> = app
            .rows
            .iter()
            .filter_map(|r| r.id().map(str::to_string))
            .collect();
        app.apply(Update::Orchestrator(Some("gone".into())));
        let after: Vec<String> = app
            .rows
            .iter()
            .filter_map(|r| r.id().map(str::to_string))
            .collect();
        assert_eq!(before, after);
    }

    /// The pin is one `[defaults]` key on the board's `routing.toml`, written
    /// through the same validated path the settings surfaces use — and aimed at
    /// the device whose board the pane is showing, not at this laptop.
    #[test]
    fn pinning_writes_the_board_config_on_the_board_host() {
        let mut app = seeded();
        app.apply(Update::BoardHostChanged {
            device: Some("the-box".into()),
            live: true,
        });
        let effects = app.run_menu_action(MenuAction::SetOrchestrator(Some("c1".into())));
        let Some(Command::Call { method, params, .. }) = effects.first() else {
            panic!("expected one call, got {effects:?}");
        };
        assert_eq!(*method, methods::WRITE_BOARD_CONFIG);
        assert_eq!(params["op"], "default");
        assert_eq!(params["key"], "orchestrator_chat");
        assert_eq!(params["value"], "c1");
        assert_eq!(params["targetDeviceId"], "the-box");

        // Unpinning sends no value at all, which is how the key is removed.
        let effects = app.run_menu_action(MenuAction::SetOrchestrator(None));
        let Some(Command::Call { params, .. }) = effects.first() else {
            panic!("expected one call, got {effects:?}");
        };
        assert!(params.get("value").is_none(), "got: {params}");
    }

    /// The board is on one device and this pane may not have found it yet.
    /// Writing the key into this laptop's own (absent) config would silently
    /// pin nothing.
    #[test]
    fn pinning_without_a_board_says_so_rather_than_writing_nowhere() {
        let mut app = seeded();
        assert!(
            app.run_menu_action(MenuAction::SetOrchestrator(Some("c1".into())))
                .is_empty()
        );
        assert!(app.notice.is_some());
    }

    /// The same item does both, because whoever wants the notices to stop
    /// reaches for the row they pinned.
    #[test]
    fn the_menu_offers_unpinning_on_the_row_that_is_pinned() {
        let mut app = seeded();
        app.apply(Update::Orchestrator(Some("c1".into())));
        let at = app
            .rows
            .iter()
            .position(|row| row.id() == Some("c1"))
            .expect("c1 is on screen");
        app.cursor = at;
        app.open_context_menu(0, 0);
        let Some(Overlay::Menu { items, .. }) = &app.overlay else {
            panic!("no menu");
        };
        let pin = items
            .iter()
            .find(|i| i.label.contains("orchestrator"))
            .expect("the pin item");
        assert_eq!(pin.label, "Unpin as orchestrator");
        assert_eq!(pin.action, MenuAction::SetOrchestrator(None));
    }

    fn board_device(id: &str, name: &str, created: &str) -> Device {
        Device {
            id: id.into(),
            name: name.into(),
            platform: "linux".into(),
            last_seen_at: None,
            created_at: Some(
                DateTime::parse_from_rfc3339(created)
                    .unwrap()
                    .with_timezone(&Utc),
            ),
            version: None,
        }
    }

    /// gh#55: `d` walks automatic → this device → each other device → back,
    /// and every step is a command the supervisor acts on.
    #[test]
    fn d_cycles_which_device_hosts_the_board() {
        let mut app = seeded();
        app.apply(Update::LocalDevice("laptop".into()));
        app.apply(Update::Devices(vec![
            board_device("box", "the-box", "2026-01-02T00:00:00Z"),
            board_device("laptop", "laptop", "2026-01-01T00:00:00Z"),
        ]));
        app.act(Action::ToggleBoard);
        assert_eq!(app.board_pin, BoardHost::Auto, "automatic is the default");

        let effects = app.act(Action::BoardCycleHost);
        assert_eq!(app.board_pin, BoardHost::Pinned(None));
        assert!(matches!(
            effects.first(),
            Some(Command::BoardHost(BoardHost::Pinned(None)))
        ));
        assert!(
            app.notice.as_ref().is_some_and(|n| n.text.contains("this device")),
            "the step has to say where it landed"
        );

        app.act(Action::BoardCycleHost);
        assert_eq!(app.board_pin, BoardHost::Pinned(Some("box".into())));
        assert!(
            app.notice.as_ref().is_some_and(|n| n.text.contains("the-box")),
            "a device is named, not id'd"
        );

        // One more press always gets you back out — the same promise `f` makes.
        app.act(Action::BoardCycleHost);
        assert_eq!(app.board_pin, BoardHost::Auto);
    }

    /// The header only speaks up when there is something to say: this device
    /// serving its own board is the ordinary case and stays silent.
    #[test]
    fn the_board_header_names_a_remote_host_and_nothing_else() {
        let mut app = seeded();
        app.apply(Update::LocalDevice("laptop".into()));
        app.apply(Update::Devices(vec![board_device(
            "box",
            "the-box",
            "2026-01-02T00:00:00Z",
        )]));
        app.apply(Update::BoardHostChanged {
            device: None,
            live: true,
        });
        assert_eq!(app.board_host_note(), None);

        // Still sweeping: say who is being asked, do not claim an answer.
        app.apply(Update::BoardHostChanged {
            device: Some("box".into()),
            live: false,
        });
        assert_eq!(app.board_host_note().as_deref(), Some("asking the-box…"));

        app.apply(Update::BoardHostChanged {
            device: Some("box".into()),
            live: true,
        });
        assert_eq!(app.board_host_note().as_deref(), Some("on the-box"));
    }

    #[test]
    fn enter_on_a_working_row_opens_its_chat() {
        let mut app = seeded();
        app.apply(Update::Chats(vec![chat("chat-1", "s1", 1)]));
        app.act(Action::ToggleBoard);
        let mut working = board_row("w", BoardState::Working);
        working.chat_id = Some("chat-1".into());
        app.apply(Update::Board(vec![working]));
        app.board.selected = Some("w".into());

        let effects = app.act(Action::BoardEnter);
        assert_eq!(app.selected_chat.as_deref(), Some("chat-1"));
        assert!(!app.board_open, "the board gives way to the chat");
        assert!(
            effects
                .iter()
                .any(|c| is_watch(c, Some("chat-1"))),
            "the chat's transcript must stream"
        );
    }

    #[test]
    fn enter_on_a_section_header_folds_it() {
        let mut app = seeded();
        app.act(Action::ToggleBoard);
        app.apply(Update::Board(vec![
            board_row("1", BoardState::Ready),
            board_row("2", BoardState::Ready),
        ]));
        let header = crate::board::section_row_id(BoardState::Ready);
        app.board.selected = Some(header.clone());
        let lines_before = app.board.lines().len();
        assert!(app.act(Action::BoardEnter).is_empty());
        assert_eq!(app.board.lines().len(), lines_before - 2, "rows fold away");
        assert!(app.board.is_collapsed(BoardState::Ready));
        // And the header stays, or it could never be reopened.
        assert!(app.board.lines().iter().any(|l| l.id() == header));
    }

    #[test]
    fn f_cycles_the_routes_then_back_to_all() {
        let mut app = seeded();
        app.act(Action::ToggleBoard);
        let mut a = board_row("a", BoardState::Ready);
        a.route = Some("offhand".into());
        let mut b = board_row("b", BoardState::Ready);
        b.route = Some("tally".into());
        let mut u = board_row("u", BoardState::Ready);
        u.route = None;
        u.dispatchable = false;
        app.apply(Update::Board(vec![a, b, u]));

        app.act(Action::BoardCycleFilter);
        assert_eq!(app.board.filter, comet_proto::view::board::Filter::Route("offhand".into()));
        app.act(Action::BoardCycleFilter);
        assert_eq!(app.board.filter, comet_proto::view::board::Filter::Route("tally".into()));
        app.act(Action::BoardCycleFilter);
        assert_eq!(app.board.filter, comet_proto::view::board::Filter::NoRoute);
        app.act(Action::BoardCycleFilter);
        assert_eq!(app.board.filter, comet_proto::view::board::Filter::All, "wraps to everything");

        // The cursor never rests on a hidden row.
        app.act(Action::BoardCycleFilter); // offhand
        app.board.selected = Some("b".into());
        app.act(Action::BoardCycleFilter); // tally — b stays
        app.act(Action::BoardCycleFilter); // no route — b leaves
        assert_eq!(app.board.selected.as_deref(), Some("u"), "onto a shown row");
    }

    #[test]
    fn find_filters_as_you_type_and_esc_clears() {
        let mut app = seeded();
        app.act(Action::ToggleBoard);
        app.apply(Update::Board(vec![
            board_row("1", BoardState::Ready),
            board_row("2", BoardState::Working),
        ]));
        app.board.selected = Some("1".into());

        app.act(Action::BoardFind);
        assert!(app.board.typing);
        for ch in "task 2".chars() {
            app.act(Action::BoardFindType(ch));
        }
        // "task 2" only matches row 2, so the cursor moves onto it.
        assert_eq!(app.board.selected_task().map(|r| r.id.as_str()), Some("2"));
        assert_eq!(app.board.shown_tasks(), 1);

        app.act(Action::BoardFindEscape);
        assert!(!app.board.typing);
        assert_eq!(app.board.filter, comet_proto::view::board::Filter::All, "esc clears the query");
        assert_eq!(app.board.shown_tasks(), 2);
    }

    #[test]
    fn selection_survives_a_refresh_that_reorders_the_list() {
        let mut app = seeded();
        app.act(Action::ToggleBoard);
        app.apply(Update::Board(vec![board_row("a", BoardState::Ready)]));
        app.board.selected = Some("a".into());
        // The row stays where it is, just re-ordered in the stream.
        app.apply(Update::Board(vec![
            board_row("z", BoardState::Working),
            board_row("a", BoardState::Ready),
        ]));
        assert_eq!(app.board.selected.as_deref(), Some("a"));
    }

    // -----------------------------------------------------------------------
    // The sidebar's Agents section (gh#103)
    // -----------------------------------------------------------------------

    /// The rows a live board attempt puts in the sidebar, with the board pane
    /// never opened — presence must not be something you have to go and find.
    fn agent_rows_in_sidebar(app: &App) -> Vec<(String, AgentState)> {
        app.rows
            .iter()
            .filter_map(|row| match row {
                Row::Agent {
                    identifier, state, ..
                } => Some((identifier.clone(), *state)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn live_attempts_show_in_the_sidebar_without_opening_the_board() {
        let mut app = seeded();
        app.apply(Update::Chats(vec![
            chat("chat-a", "s1", 1),
            chat("chat-b", "s1", 2),
            chat("chat-c", "s1", 3),
        ]));
        let mut a = board_row("a", BoardState::Working);
        a.chat_id = Some("chat-a".into());
        let mut b = board_row("b", BoardState::Working);
        b.chat_id = Some("chat-b".into());
        let mut c = board_row("c", BoardState::Blocked);
        c.chat_id = Some("chat-c".into());
        app.apply(Update::Board(vec![a, b, c]));
        app.apply(Update::Sessions(vec![
            session("chat-a", SessionStatus::Working, 0),
            session("chat-b", SessionStatus::Working, 0),
            session("chat-c", SessionStatus::AwaitingInput, 0),
        ]));

        assert!(!app.board_open, "nobody opened the board pane");
        let agents = agent_rows_in_sidebar(&app);
        assert_eq!(agents.len(), 3, "three agents running, three rows");
        // Blocked floats, and the header says how many.
        assert_eq!(agents[0], ("gh#c".to_string(), AgentState::Blocked));
        assert!(app.rows.iter().any(|row| matches!(
            row,
            Row::Section { label, action: Some(count) }
                if label == "Agents" && count == "1 blocked"
        )));
    }

    /// The section is the *live* attempts and only those: it appears with the
    /// first dispatch and goes when the attempt ends, with no other cleanup.
    #[test]
    fn the_section_appears_and_leaves_with_the_attempt() {
        let mut app = seeded();
        app.apply(Update::Chats(vec![chat("chat-a", "s1", 1)]));
        assert!(agent_rows_in_sidebar(&app).is_empty(), "nothing dispatched");

        let mut live = board_row("a", BoardState::Working);
        live.chat_id = Some("chat-a".into());
        app.apply(Update::Board(vec![live]));
        assert_eq!(agent_rows_in_sidebar(&app).len(), 1);

        // Settled: the attempt closed, so the row kept neither its state nor its
        // chat. The chat itself stays findable under its space.
        app.apply(Update::Board(vec![board_row("a", BoardState::Review)]));
        assert!(agent_rows_in_sidebar(&app).is_empty());
        assert!(
            app.rows.iter().any(|row| row.id() == Some("chat-a")),
            "the session row outlives the attempt"
        );
        assert!(
            !app.rows
                .iter()
                .any(|row| matches!(row, Row::Section { label, .. } if label == "Agents")),
            "an empty Agents header is a permanent reminder of nothing"
        );
    }

    /// An agent row and its session row address the same chat. The cursor has to
    /// be able to tell them apart, or a rebuild slides it between the two lists.
    #[test]
    fn the_cursor_does_not_slide_between_an_agent_and_its_session_row() {
        let mut app = seeded();
        app.apply(Update::Chats(vec![chat("chat-a", "s1", 1)]));
        let mut live = board_row("a", BoardState::Working);
        live.chat_id = Some("chat-a".into());
        app.apply(Update::Board(vec![live.clone()]));

        // Park on the session row, then let a board frame rebuild the sidebar.
        app.cursor = app
            .rows
            .iter()
            .position(|row| matches!(row, Row::Chat { id, .. } if id == "chat-a"))
            .unwrap();
        app.apply(Update::Board(vec![live]));
        assert!(
            matches!(app.rows.get(app.cursor), Some(Row::Chat { .. })),
            "the cursor stayed in the Sessions list"
        );

        // And from the agent row, the same in reverse.
        app.cursor = app
            .rows
            .iter()
            .position(|row| matches!(row, Row::Agent { .. }))
            .unwrap();
        let mut again = board_row("a", BoardState::Blocked);
        again.chat_id = Some("chat-a".into());
        app.apply(Update::Board(vec![again]));
        assert!(matches!(app.rows.get(app.cursor), Some(Row::Agent { .. })));
    }

    #[test]
    fn opening_an_agent_row_opens_its_chat() {
        let mut app = seeded();
        app.apply(Update::Chats(vec![
            chat("chat-a", "s1", 1),
            chat("chat-b", "s2", 2),
        ]));
        app.apply(Update::Spaces(vec![
            space("s1", "/dev/one", 0),
            space("s2", "/dev/two", 1),
        ]));
        let mut live = board_row("a", BoardState::Blocked);
        live.chat_id = Some("chat-b".into());
        app.apply(Update::Board(vec![live]));
        app.selected_space = Some("s1".into());

        app.cursor = app
            .rows
            .iter()
            .position(|row| matches!(row, Row::Agent { .. }))
            .unwrap();
        let effects = app.act(Action::Open);
        assert_eq!(app.selected_chat.as_deref(), Some("chat-b"));
        // Answering a blocked agent means typing at it, so focus lands there.
        assert_eq!(app.focus, Focus::Composer);
        assert_eq!(
            app.selected_space.as_deref(),
            Some("s2"),
            "opening an agent lands in the space its dispatch routed to"
        );
        assert!(effects.iter().any(|c| is_watch(c, Some("chat-b"))));
    }

    /// The counter has to move on its own: a blocked agent animates nothing, so
    /// without this its age freezes at whatever the last frame caught.
    #[test]
    fn a_live_agent_row_owes_the_loop_a_second_by_second_redraw() {
        let mut app = seeded();
        app.apply(Update::Auth(Box::new(AuthState::SignedIn {
            user: comet_proto::UserProfile {
                id: "u".into(),
                email: "b@example.com".into(),
                name: None,
            },
            org_id: Some("org".into()),
        })));
        app.connection = ConnectionStatus::Ready;
        app.apply(Update::Chats(vec![chat("chat-a", "s1", 1)]));
        assert!(!app.counting(), "nothing running, no wake-ups owed");

        let mut live = board_row("a", BoardState::Blocked);
        live.chat_id = Some("chat-a".into());
        live.started_at = Some(Utc::now().to_rfc3339());
        app.apply(Update::Board(vec![live]));
        assert!(!app.animating(), "a blocked agent spins nothing");
        assert!(app.counting(), "but its age still has to move");
    }

    // ---- the Running group (gh#117) ---------------------------------------

    fn running_rows_in_sidebar(app: &App) -> Vec<(String, AgentState)> {
        app.rows
            .iter()
            .filter_map(|row| match row {
                Row::Running { title, state, .. } => Some((title.clone(), *state)),
                _ => None,
            })
            .collect()
    }

    /// The gh#117 case exactly: an orchestrator raises in-chat subagents rather
    /// than dispatching, so real work runs with no attempt row anywhere. It has
    /// to be visible without a board having heard of it.
    #[test]
    fn a_working_chat_the_board_never_released_still_shows() {
        let mut app = seeded();
        app.apply(Update::Chats(vec![
            chat("orchestrator", "s1", 1),
            chat("adhoc", "s1", 2),
            chat("quiet", "s1", 3),
        ]));
        app.apply(Update::Sessions(vec![
            session("orchestrator", SessionStatus::Working, 0),
            session("adhoc", SessionStatus::AwaitingInput, 0),
        ]));

        assert!(!app.board_open, "nobody opened the board pane");
        assert!(
            app.board.rows.is_empty(),
            "and there is no board here to have released them"
        );
        let running = running_rows_in_sidebar(&app);
        assert_eq!(
            running,
            vec![
                ("chat adhoc".to_string(), AgentState::Blocked),
                ("chat orchestrator".to_string(), AgentState::Working),
            ],
            "blocked floats, and the quiet chat is not a run"
        );
        assert!(app.rows.iter().any(|row| matches!(
            row,
            Row::Section { label, action: Some(count) }
                if label == "Running" && count == "1 blocked"
        )));
    }

    /// The two groups partition the box's load — a dispatched chat is the
    /// Agents group's, and drawing it twice would double-count what is running.
    #[test]
    fn a_dispatched_chat_is_an_agent_and_not_also_running() {
        let mut app = seeded();
        app.apply(Update::Chats(vec![
            chat("chat-a", "s1", 1),
            chat("chat-b", "s1", 2),
        ]));
        let mut live = board_row("a", BoardState::Working);
        live.chat_id = Some("chat-a".into());
        app.apply(Update::Board(vec![live]));
        app.apply(Update::Sessions(vec![
            session("chat-a", SessionStatus::Working, 0),
            session("chat-b", SessionStatus::Working, 0),
        ]));

        assert_eq!(
            agent_rows_in_sidebar(&app)
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>(),
            vec!["gh#a".to_string()]
        );
        assert_eq!(
            running_rows_in_sidebar(&app)
                .into_iter()
                .map(|(title, _)| title)
                .collect::<Vec<_>>(),
            vec!["chat chat-b".to_string()]
        );
    }

    /// The group appears with the run and leaves with it, within one watch
    /// frame either way — and leaves nothing behind when it goes.
    #[test]
    fn the_running_group_appears_and_leaves_with_the_run() {
        let mut app = seeded();
        app.apply(Update::Chats(vec![chat("chat-a", "s1", 1)]));
        assert!(running_rows_in_sidebar(&app).is_empty(), "nothing running");

        app.apply(Update::Sessions(vec![session(
            "chat-a",
            SessionStatus::Working,
            0,
        )]));
        assert_eq!(running_rows_in_sidebar(&app).len(), 1);

        app.apply(Update::Sessions(vec![session(
            "chat-a",
            SessionStatus::Idle,
            0,
        )]));
        assert!(running_rows_in_sidebar(&app).is_empty());
        assert!(
            app.rows.iter().any(|row| row.id() == Some("chat-a")),
            "the session row outlives the run"
        );
        assert!(
            !app.rows
                .iter()
                .any(|row| matches!(row, Row::Section { label, .. } if label == "Running")),
            "an empty Running header is a permanent reminder of nothing"
        );
    }

    /// A crashed backend sends no frame to say it died, so the row has to
    /// expire against the clock — the same staleness gate the dots use.
    #[test]
    fn a_dead_run_leaves_the_group_on_staleness_alone() {
        let mut app = seeded();
        app.apply(Update::Chats(vec![chat("chat-a", "s1", 1)]));
        app.apply(Update::Sessions(vec![session(
            "chat-a",
            SessionStatus::Working,
            (comet_proto::view::SESSION_STALE_MS / 1000) + 1,
        )]));
        assert!(
            running_rows_in_sidebar(&app).is_empty(),
            "a session nothing has heartbeated is not a live run"
        );
    }

    /// A running row and its session row address the same chat, so the cursor
    /// has to be able to tell them apart across a rebuild.
    #[test]
    fn the_cursor_does_not_slide_between_a_running_row_and_its_session_row() {
        let mut app = seeded();
        app.apply(Update::Chats(vec![chat("chat-a", "s1", 1)]));
        app.apply(Update::Sessions(vec![session(
            "chat-a",
            SessionStatus::Working,
            0,
        )]));
        app.cursor = app
            .rows
            .iter()
            .position(|row| matches!(row, Row::Running { .. }))
            .unwrap();
        app.apply(Update::Sessions(vec![session(
            "chat-a",
            SessionStatus::AwaitingInput,
            0,
        )]));
        assert!(matches!(app.rows.get(app.cursor), Some(Row::Running { .. })));
    }

    #[test]
    fn opening_a_running_row_opens_its_chat_in_its_space() {
        let mut app = seeded();
        app.apply(Update::Chats(vec![
            chat("chat-a", "s1", 1),
            chat("chat-b", "s2", 2),
        ]));
        app.apply(Update::Spaces(vec![
            space("s1", "/dev/one", 0),
            space("s2", "/dev/two", 1),
        ]));
        app.apply(Update::Sessions(vec![session(
            "chat-b",
            SessionStatus::AwaitingInput,
            0,
        )]));
        app.selected_space = Some("s1".into());

        app.cursor = app
            .rows
            .iter()
            .position(|row| matches!(row, Row::Running { .. }))
            .unwrap();
        let effects = app.act(Action::Open);
        assert_eq!(app.selected_chat.as_deref(), Some("chat-b"));
        // Answering a blocked run means typing at it, so focus lands there.
        assert_eq!(app.focus, Focus::Composer);
        assert_eq!(app.selected_space.as_deref(), Some("s2"));
        assert!(effects.iter().any(|c| is_watch(c, Some("chat-b"))));
    }

    /// A blocked unmanaged run animates nothing, and its age still has to move.
    #[test]
    fn a_running_row_owes_the_loop_a_second_by_second_redraw() {
        let mut app = seeded();
        app.apply(Update::Auth(Box::new(AuthState::SignedIn {
            user: comet_proto::UserProfile {
                id: "u".into(),
                email: "b@example.com".into(),
                name: None,
            },
            org_id: Some("org".into()),
        })));
        app.connection = ConnectionStatus::Ready;
        app.apply(Update::Chats(vec![chat("chat-a", "s1", 1)]));
        assert!(!app.counting(), "nothing running, no wake-ups owed");

        app.apply(Update::Sessions(vec![Session {
            started_at: Some(Utc::now()),
            ..session("chat-a", SessionStatus::AwaitingInput, 0)
        }]));
        assert!(app.counting());
    }

    #[test]
    fn focus_cycles_skip_hidden_panes() {
        let mut app = seeded();
        app.act(Action::ToggleBoard); // board open: transcript and composer are gone
        app.focus = Focus::Composer;
        app.act(Action::FocusNext);
        assert_eq!(app.focus, Focus::Sidebar, "composer is hidden, so it is skipped");
        app.focus = Focus::Board;
        app.act(Action::FocusNext);
        assert_eq!(app.focus, Focus::Sidebar, "the chat panes are hidden when the board is open");
    }
}
