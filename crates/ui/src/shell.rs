//! The app shell (comet `__root.tsx`): sidebar column + main panel + optional
//! right "Changes" pane, plus the boot splash and the connection gate.
//!
//! Layout is comet's: collapsible drag-resizable sidebar (208–400px, default
//! 256) with a 200ms ease-out width transition; main panel with an h-11 header,
//! content outlet, and a reserved h-6 status strip so later content never
//! shifts; right pane scaffold (360–760px, default 520), hidden by default.
//! Widths/collapsed state persist to `ui-settings.json` (debounced).
//!
//! Resize handles use gpui's drag-and-drop pattern (an `on_drag` with an empty
//! ghost view + `on_drag_move::<Marker>` on the root), the same idiom as Zed's
//! dock. Double-clicking a handle resets that pane to its default width.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::Utc;
use gpui::{
    AnyElement, App, ClipboardItem, Context, Empty, Entity, Focusable as _, IntoElement,
    KeyBinding, Keystroke, MouseButton, MouseDownEvent, MouseUpEvent, Pixels, Point, Render,
    SharedString, Subscription, Task, Window, WindowControlArea, actions, div, prelude::*, px,
};

use comet_proto::view::account as view_account;
use comet_rpc::methods;
use gpui_tokio::Tokio;

use crate::board::{BoardEvent, BoardPanel, ToggleBoard};
use crate::changes::Changes;
use crate::commands::{self, NewSession, NewSessionIntent};
use crate::composer::{Composer, ComposerEvent, ComposerInput, ComposerInputEvent};
use crate::icons::{self, icon};
use crate::loaders;
use crate::motion::{self, AnimationExt as _, RESIZE, SPLASH_OUT};
use crate::popover::{self, Loadable};
use crate::rail;
use crate::review::{ReviewEvent, ReviewPanel};
use crate::settings::accounts::AccountsPage;
use crate::settings::appearance::{AppearanceEvent, AppearancePage};
use crate::settings::archived::ArchivedPage;
use crate::settings::devices::DevicesPage;
use crate::settings::members::MembersPage;
use crate::settings::automations::AutomationsPage;
use crate::settings::routing::RoutingPage;
use crate::settings::shortcuts::{ShortcutsEvent, ShortcutsPage};
use crate::settings::stats::StatsPage;
use crate::settings::{
    KeymapConfig, REVIEW_SESSION_DEFAULT, REVIEW_SESSION_MAX, REVIEW_SESSION_MIN,
    RIGHT_PANE_DEFAULT, RIGHT_PANE_MAX, RIGHT_PANE_MIN, SAVE_DEBOUNCE_MS, SIDEBAR_DEFAULT,
    SIDEBAR_MAX, SIDEBAR_MIN, TERMINAL_DEFAULT_HEIGHT, UiSettings, platform_combo,
};
use crate::state::{
    AppState, ConnectionStatus, EngineBootConfig, GatePhase, Indicator, OrgRow, format_time_ago,
    org_name_valid, parse_orgs, sort_memberships,
};
use crate::terminal::panel::{TerminalPanel, ToggleTerminal, clamp_terminal_height};
use crate::theme::{Bed, ListRow as _, Theme};
use crate::transcript::{self, Transcript};

mod account;
mod fork;
// `pub(crate)` for one thing only: `spaces::status_dot_color`, the sidebar's
// half of the status-colour contract the board pane's test asserts (gh#173).
pub(crate) mod spaces;
mod tabs;

use fork::ForkDialog;
use spaces::{AddSpaceFlow, RenameSpaceDialog};

actions!(shell, [ToggleSidebar, ToggleChanges, AddSpacePalette]);

// ---------------------------------------------------------------------------
// Traffic-light-aware titlebar layout (feature-inventory §1.1)
// ---------------------------------------------------------------------------

/// Where the top-left window-control cluster starts, in px from the window's
/// left edge (comet window-controls.tsx: `left: fullscreen ? 12 : 88`). The
/// frameless hiddenInset chrome puts the macOS traffic lights at {14,15};
/// fullscreen hides them and the cluster reclaims the inset.
pub fn titlebar_cluster_start(fullscreen: bool) -> f32 {
    if fullscreen { 12.0 } else { 88.0 }
}

/// Width of the spacer ahead of the control cluster for a strip that already
/// carries `container_pad` px of its own left padding. macOS only — on
/// Linux/Windows there are no traffic lights and the cluster hugs the edge.
pub fn titlebar_spacer_width(is_macos: bool, fullscreen: bool, container_pad: f32) -> f32 {
    if !is_macos {
        return 0.0;
    }
    (titlebar_cluster_start(fullscreen) - container_pad).max(0.0)
}

/// Width of the persistent top-left button cluster itself (sidebar toggle +
/// back/forward: three 24px buttons, 2px gaps).
pub const CLUSTER_BUTTONS_WIDTH: f32 = 24.0 * 3.0 + 2.0 * 2.0;

/// Where the cluster's first button starts, from the window's left edge.
pub fn cluster_buttons_start(is_macos: bool, fullscreen: bool) -> f32 {
    if is_macos {
        titlebar_cluster_start(fullscreen)
    } else {
        10.0
    }
}

/// Left clearance a full-bleed header (collapsed sidebar) needs so its content
/// starts past the overlay cluster, given the header's own `container_pad`.
pub fn cluster_clearance(is_macos: bool, fullscreen: bool, container_pad: f32) -> f32 {
    (cluster_buttons_start(is_macos, fullscreen) + CLUSTER_BUTTONS_WIDTH + 8.0 - container_pad)
        .max(0.0)
}

/// (Re-)apply the whole app keymap: clears every binding, restores the composer
/// map, then binds the customizable shortcuts from `keymap` (feature-inventory
/// §1.4). Invalid persisted combos fall back to that shortcut's default.
pub fn apply_keymap(cx: &mut App, keymap: &KeymapConfig) {
    fn valid_or_default(combo: &str, fallback: &str) -> String {
        let candidate = platform_combo(combo);
        if Keystroke::parse(&candidate).is_ok() {
            candidate
        } else {
            tracing::warn!(%combo, "unparseable shortcut combo; using default");
            platform_combo(fallback)
        }
    }
    cx.clear_key_bindings();
    crate::composer::init(cx);
    // Fixed app-level shortcuts (⌘Q quit, ⌘W close, ⌘M minimize, ⌘H hide) —
    // these back the native menu key equivalents and must survive keymap
    // re-application.
    crate::app_menus::bind_keys(cx);
    cx.bind_keys([
        commands::new_session_binding(keymap),
        KeyBinding::new(
            &valid_or_default(&keymap.toggle_sidebar, "mod-s"),
            ToggleSidebar,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(&keymap.toggle_changes, "mod-b"),
            ToggleChanges,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(&keymap.toggle_terminal, "mod-j"),
            ToggleTerminal,
            None,
        ),
        KeyBinding::new(
            &valid_or_default(&keymap.toggle_board, "mod-shift-b"),
            ToggleBoard,
            None,
        ),
        // Fixed: ⌘K summons the add-space palette (the ⌘K chip in its search
        // bar); pressing it again dismisses.
        KeyBinding::new(&platform_combo("mod-k"), AddSpacePalette, None),
    ]);
}

/// The settings sections (feature-inventory §1.5 routes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    Devices,
    Agents,
    Members,
    /// The board's `routing.toml` (gh#75) — a comet-board addition, and the
    /// only settings section whose subject lives on another device.
    Routing,
    /// The board's auto-pick rules (gh#490) — a comet-board addition, board-
    /// hosted like [`Self::Routing`]: the rules are `routing.toml`, the
    /// history is `board.db`, and both live on whichever device hosts the
    /// board.
    Automations,
    /// What the board did with the work it was given (gh#143) — a
    /// comet-board addition, and like [`Self::Routing`] its subject lives on
    /// whichever device hosts the board.
    Stats,
    Appearance,
    Shortcuts,
    Archived,
}

impl SettingsSection {
    /// Nav order, from the supplied Settings design file (gh#258).
    ///
    /// The two board-hosted pages sit AFTER the four that are about this
    /// device and this window, not between them: Devices/Agents/Members are
    /// who and what is signed in here, Appearance/Shortcuts are how this app
    /// behaves, and Routing/Stats are about a board that may live on another
    /// machine entirely. Archived closes the list, as the tail of it always
    /// has.
    pub const ALL: [SettingsSection; 9] = [
        SettingsSection::Devices,
        SettingsSection::Agents,
        SettingsSection::Members,
        SettingsSection::Appearance,
        SettingsSection::Shortcuts,
        SettingsSection::Routing,
        SettingsSection::Automations,
        SettingsSection::Stats,
        SettingsSection::Archived,
    ];

    /// The nav label — **the short form** (gh#252).
    ///
    /// This is the sidebar's string and only the sidebar's: every page owns its
    /// own header (`Board stats`, `Board routing`, `Archived sessions`), which
    /// is why the nav does not have to repeat it. Four of these used to, and
    /// the nav is 256px wide: `Archived sessions` and `Board routing` were
    /// spending a third of the rail saying a word the page under the cursor was
    /// about to say again. The design's labels are one word each.
    pub fn label(self) -> &'static str {
        match self {
            SettingsSection::Devices => "Devices",
            SettingsSection::Agents => "Agents",
            // gh#76 — the workspace roster and its invitations.
            SettingsSection::Members => "Members",
            SettingsSection::Routing => "Routing",
            SettingsSection::Automations => "Automations",
            SettingsSection::Stats => "Stats",
            SettingsSection::Appearance => "Appearance",
            SettingsSection::Shortcuts => "Shortcuts",
            SettingsSection::Archived => "Archived",
        }
    }
}

/// What the main outlet shows.
///
/// [`Route::Review`] carries its subject rather than pointing at ambient
/// selection, which is why this enum is `Clone` and not `Copy`: a review is of
/// one attempt of one task, and a route that had to ask somewhere else which
/// one would be a route that can be wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Chat,
    Settings(SettingsSection),
    /// One attempt's review (gh#180) — the inverted route, where the review is
    /// the content and the chat that authored it is the reference beside it.
    ///
    /// `chat_id` is the session to show in that column, taken from the board
    /// row that opened the review. `None` where the attempt's chat is gone: a
    /// review outlives the conversation that produced it, which is half the
    /// reason the claims are recorded on the attempt at all.
    Review {
        task_id: String,
        chat_id: Option<String>,
    },
}

/// Per-chat panel open flags (comet parity: `sessionPanels` — the terminal and
/// changes panels open *per session*, in memory only; heights and every other
/// persisted setting stay global). New/unknown chats default to closed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ChatPanels {
    pub terminal_open: bool,
    pub changes_open: bool,
}

/// The session-scoped panel map. Keys are chat ids; the new-chat canvas uses
/// the empty key. Not persisted — a fresh app starts with everything closed.
#[derive(Debug, Default)]
pub struct SessionPanels {
    map: std::collections::HashMap<String, ChatPanels>,
}

impl SessionPanels {
    pub fn get(&self, key: &str) -> ChatPanels {
        self.map.get(key).copied().unwrap_or_default()
    }

    /// Flip the terminal flag for `key`; returns the new value.
    pub fn toggle_terminal(&mut self, key: &str) -> bool {
        let entry = self.map.entry(key.to_string()).or_default();
        entry.terminal_open = !entry.terminal_open;
        entry.terminal_open
    }

    /// Flip the changes flag for `key`; returns the new value.
    pub fn toggle_changes(&mut self, key: &str) -> bool {
        let entry = self.map.entry(key.to_string()).or_default();
        entry.changes_open = !entry.changes_open;
        entry.changes_open
    }
}

/// One route-history entry (comet parity: the renderer's TanStack memory
/// history — every route the user visited, browser-style).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavEntry {
    /// A chat route; the id of the selected chat ("" = the new-chat canvas).
    Chat(String),
    Settings(SettingsSection),
    /// A review route (gh#180). Carries the chat alongside the task so Back
    /// lands on the same pairing it left, rather than on whatever session
    /// happens to be selected by then.
    Review {
        task_id: String,
        chat_id: Option<String>,
    },
}

/// Browser-style navigation history for the titlebar back/forward buttons
/// (comet window-controls.tsx semantics): every route change pushes an entry;
/// Back/Forward walk the stack without changing it; pushing while behind the
/// tip truncates the entries ahead (a new branch, exactly like a browser).
#[derive(Debug)]
pub struct NavHistory {
    entries: Vec<NavEntry>,
    index: usize,
}

impl NavHistory {
    pub fn new(initial: NavEntry) -> Self {
        Self {
            entries: vec![initial],
            index: 0,
        }
    }

    pub fn current(&self) -> &NavEntry {
        &self.entries[self.index]
    }

    /// Record a route change. Re-navigating to the current route is a no-op
    /// (selecting the already-selected chat never happened as a navigation);
    /// otherwise any forward branch is truncated and the entry appended.
    pub fn push(&mut self, entry: NavEntry) {
        if *self.current() == entry {
            return;
        }
        self.entries.truncate(self.index + 1);
        self.entries.push(entry);
        self.index += 1;
    }

    /// Swap the current entry in place without growing the stack — the native
    /// equivalent of a `replace: true` navigation (comet's boot redirect from
    /// `/` into the last-used chat leaves no dead Back target behind).
    pub fn replace(&mut self, entry: NavEntry) {
        self.entries[self.index] = entry;
    }

    pub fn can_back(&self) -> bool {
        self.index > 0
    }

    /// Memory history keeps every entry, so "behind the last entry" is exactly
    /// "can go forward" (comet window-controls.tsx).
    pub fn can_forward(&self) -> bool {
        self.index + 1 < self.entries.len()
    }

    pub fn back(&mut self) -> Option<NavEntry> {
        if !self.can_back() {
            return None;
        }
        self.index -= 1;
        Some(self.current().clone())
    }

    pub fn forward(&mut self) -> Option<NavEntry> {
        if !self.can_forward() {
            return None;
        }
        self.index += 1;
        Some(self.current().clone())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Ramp height of the glass sidebar's scroll-edge fade (the gpui
/// [`gpui::EdgeFade`] scope — per-primitive, so text fades per glyph).
const SIDEBAR_GLASS_FADE_BAND: f32 = 32.0;

/// Drag marker for the sidebar resize handle.
struct SidebarResize;
/// Drag marker for the right-pane resize handle.
struct RightPaneResize;
/// Drag marker for the terminal-panel height handle.
struct TerminalResize;
/// Drag marker for the authoring session's edge on the review route (gh#180).
struct ReviewSessionResize;

/// Invisible drag ghost — resize drags render nothing at the cursor.
struct DragGhost;

impl Render for DragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// A oneshot width tween (200ms ease-out), driven MANUALLY from render via
/// [`Shell::eval_tween`] — never through a `with_animation` wrapper. gpui keys
/// an animation element's start time by its full global element-id path, so a
/// wrapper that mounts/remounts (route swap, or an ancestor animation keyed by
/// a fresh epoch) silently REPLAYS the tween from t=0. Manual evaluation keeps
/// the element tree's shape constant: a finished or stale tween is exactly the
/// steady state, no matter how the tree around it remounts (round-6 §1–3).
#[derive(Debug, Clone, Copy)]
struct WidthTween {
    from: f32,
    to: f32,
    started: std::time::Instant,
}

impl WidthTween {
    fn new(from: f32, to: f32) -> Self {
        Self {
            from,
            to,
            started: std::time::Instant::now(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SplashPhase {
    Visible,
    FadingOut,
    Gone,
}

/// The chat-row Rename dialog.
struct RenameChatDialog {
    chat_id: String,
    input: Entity<ComposerInput>,
    /// Focus the input on the dialog's first paint (opened without window access).
    focus_pending: bool,
    _events: Subscription,
}

/// In-app update lifecycle (macOS bundle installs; see `render_update_strip`).
enum UpdateFlow {
    Idle,
    Downloading,
    /// Staged bundle ready to swap in — one click restarts into it.
    Ready(PathBuf),
    Failed(SharedString),
}

/// The "Create your workspace" gate (feature-inventory §1.2 OrgGate).
struct OrgGateUi {
    name_input: Entity<ComposerInput>,
    /// The invitation code an invited teammate pastes (gh#76). The gate is
    /// where it belongs: someone invited into an existing workspace lands here
    /// with no org of their own, and creating one would be the wrong answer.
    invite_input: Entity<ComposerInput>,
    orgs: Loadable<Vec<OrgRow>>,
    submitting: bool,
    error: Option<SharedString>,
    task: Option<Task<()>>,
    _events: Subscription,
    _invite_events: Subscription,
}

pub struct Shell {
    state: Entity<AppState>,
    transcript: Entity<Transcript>,
    composer: Entity<Composer>,
    /// External file drag hovering the conversation column — shows the
    /// "Drop images to attach" veil over the whole chat area; a drop stages
    /// the files in the composer.
    file_drag_active: bool,
    /// Lazy panes: no entity (and no RPC) until first opened.
    terminal: Option<Entity<TerminalPanel>>,
    changes: Option<Entity<Changes>>,
    /// The task board (§gh#70). Unlike the terminal and changes
    /// panes it is a GLOBAL view — the queue across every workspace — so its
    /// open flag lives on the shell, not in [`SessionPanels`]. And unlike them
    /// it is **not** lazy: its `WatchBoard` stream feeds the sidebar's Agents
    /// section (gh#103), which is on screen with the dock shut.
    board: Entity<BoardPanel>,
    board_open: bool,
    /// The review card on [`Route::Review`] (gh#180). Lazy like the other
    /// panes, and REPLACED rather than re-pointed when the route moves to a
    /// different task: a reply for the review you just left must have nowhere
    /// to land.
    review: Option<Entity<ReviewPanel>>,
    /// The live review card's event subscription (§gh#238's `Read the diff`).
    /// Replaced with the panel, so a dropped card's subscription goes with it.
    review_events: Option<Subscription>,
    /// Repaints the shell when the review card's state moves — see
    /// [`Shell::render_delivery_preview`].
    review_observation: Option<Subscription>,
    /// Chat outlet vs settings pages vs one attempt's review.
    route: Route,
    /// Route history behind the titlebar back/forward buttons (§ nav history).
    nav: NavHistory,
    devices_page: Option<Entity<DevicesPage>>,
    members_page: Option<Entity<MembersPage>>,
    routing_page: Option<Entity<RoutingPage>>,
    automations_page: Option<Entity<AutomationsPage>>,
    stats_page: Option<Entity<StatsPage>>,
    archived_page: Option<Entity<ArchivedPage>>,
    shortcuts_page: Option<Entity<ShortcutsPage>>,
    accounts_page: Option<Entity<AccountsPage>>,
    appearance_page: Option<Entity<AppearancePage>>,
    shortcuts_sub: Option<Subscription>,
    appearance_sub: Option<Subscription>,
    /// Session-row context menu: (chat id, window position).
    chat_menu: Option<(String, Point<Pixels>)>,
    rename_dialog: Option<RenameChatDialog>,
    /// The fork menu (gh#425): open on a message of the selected chat.
    fork_dialog: Option<ForkDialog>,
    /// The fork call itself, and the harness probe that precedes it.
    fork_task: Option<Task<()>>,
    fork_models_task: Option<Task<()>>,
    /// Chat id awaiting delete confirmation.
    delete_confirm: Option<String>,
    /// Space-row context menu: (space id, window position).
    space_menu: Option<(String, Point<Pixels>)>,
    rename_space_dialog: Option<RenameSpaceDialog>,
    /// Space id awaiting delete confirmation (hard delete + session cascade).
    delete_space_confirm: Option<String>,
    /// The add-space palette (⌘K-style; device tabs + folder search), `Some`
    /// while open.
    add_space: Option<AddSpaceFlow>,
    /// Last selected chat per space (in-memory, like [`SessionPanels`]) — a
    /// space switch lands back on the tab you left.
    space_last_chat: std::collections::HashMap<String, String>,
    /// Session tab currently hovered (close button appears on hover).
    tab_hover: Option<String>,
    /// Session-tab drag-reorder in flight (see `tabs::TabDragState`).
    tab_drag: Option<tabs::TabDragState>,
    /// Space-row drag-reorder in flight (see `spaces::SpaceDragState`).
    space_drag: Option<spaces::SpaceDragState>,
    /// Scroll position of the session tab region (drives the edge fades and
    /// the drop-index math under horizontal overflow).
    tabs_scroll: gpui::ScrollHandle,
    /// Chat id last auto-scrolled into view — scroll-to-selected fires once per
    /// selection change, not every frame (which would fight manual scrolling).
    tabs_scrolled_to: Option<String>,
    /// Scroll position of the sidebar lists region (drives its edge fades).
    sidebar_scroll: gpui::ScrollHandle,
    /// `settings.last_space_id` applied once after the first spaces frame.
    space_boot_applied: bool,
    /// Last seen session status per chat — the chime trigger compares against
    /// it (a row's FIRST appearance never chimes, so boot stays silent).
    sound_prev: std::collections::HashMap<String, comet_proto::SessionStatus>,
    user_menu_open: bool,
    /// Outside-click dismissal instant — suppresses the trigger click that
    /// follows the same mouse-down from instantly reopening the menu.
    user_menu_dismissed_at: Option<std::time::Instant>,
    /// Inline sidebar error strip (mutation failures); click dismisses.
    sidebar_notice: Option<SharedString>,
    /// Local lifecycle of an in-app update (macOS bundle swap) — the engine's
    /// UpdateStatus stream says WHETHER one exists; this says how far the
    /// download/stage of it has come in this process.
    update_flow: UpdateFlow,
    update_task: Option<Task<()>>,
    /// Version whose update strip the user dismissed (advisory installs only —
    /// a newer release shows the strip again).
    update_dismissed: Option<String>,
    /// How this binary was installed — decides the strip's click behavior.
    /// Cached: `detect_install` stats `current_exe` and this renders per frame.
    install: comet_update::InstallKind,
    org: Option<OrgGateUi>,
    mutate_task: Option<Task<()>>,
    /// One New Session invocation at a time; also absorbs key-repeat events.
    new_session_task: Option<Task<()>>,
    /// GPUI resolves a key binding before element capture sees `is_held`.
    /// Debounce the resolved action itself so a fast RPC cannot let a held
    /// physical shortcut become a second invocation.
    last_new_session_action: Option<Instant>,
    /// Durable until the exact row returns through WATCH_CHATS.
    new_session_intent: Option<NewSessionIntent>,
    new_session_intent_attempted: bool,
    /// True only after createChat returned from an exact durable workspace
    /// snapshot. WATCH_CHATS alone is not a commit acknowledgement.
    new_session_intent_confirmed: bool,
    /// Highlighted row in the keyboard-operable no-context chooser.
    new_session_chooser: Option<usize>,
    focus_composer_next_render: bool,
    auth_task: Option<Task<()>>,
    /// Kept for the failed-gate "Retry" action.
    boot: EngineBootConfig,
    data_dir: PathBuf,
    settings: UiSettings,
    /// Session-scoped panel open flags (terminal / changes per chat; §1.10-1.11
    /// parity — heights stay in [`UiSettings`]).
    panels: SessionPanels,
    /// The panel key of the chat currently shown ("" = new-chat canvas).
    active_chat: String,
    /// Membership fingerprint (space ids + device ids) of the last slug sweep —
    /// the trigger for re-asking hosts for `space → repo` links (gh#124).
    slug_sweep_seen: Option<Vec<String>>,
    /// The sweep in flight; dropping it cancels (one sweep at a time).
    slug_task: Option<Task<()>>,
    /// The chat selection last revealed in the sidebar — edge detector for
    /// auto-expanding the selected chat's space (a later manual collapse of
    /// that space is respected until the selection moves again).
    revealed_chat: Option<String>,
    /// Dev/testing knobs (`COMET_OPEN_DIALOG`, `COMET_FORCE_GATE`) — see
    /// [`Shell::new`].
    debug_dialog: Option<String>,
    debug_gate: Option<GatePhase>,
    sidebar_tween: Option<WidthTween>,
    right_tween: Option<WidthTween>,
    terminal_tween: Option<WidthTween>,
    /// Last observed `window.is_fullscreen()` (`None` before first paint) —
    /// flips key the traffic-light inset tween.
    fullscreen: Option<bool>,
    /// 200ms ease-out tween of the cluster start on fullscreen toggles.
    titlebar_tween: Option<WidthTween>,
    /// Armed by mouse-down on a titlebar strip; the next mouse-move hands the
    /// drag to the compositor (zed's platform-titlebar pattern).
    titlebar_should_move: bool,
    /// Clears the height tween once it completes (so a closed panel unmounts).
    terminal_tween_task: Option<Task<()>>,
    /// Height-drag anchor: (pointer y, height) at mouse-down on the handle.
    terminal_drag_anchor: Option<(f32, f32)>,
    /// `motion::reduced_motion` snapshot, refreshed at the top of each render
    /// pass so [`Shell::eval_tween`] (called from `&self` render helpers) can
    /// snap without a `cx`.
    reduced_motion: bool,
    /// Set by [`Shell::eval_tween`] when any tween is mid-flight this frame;
    /// render schedules the next animation frame off it.
    motion_active: std::cell::Cell<bool>,
    splash: SplashPhase,
    splash_task: Option<Task<()>>,
    save_task: Option<Task<()>>,
    /// Focus fallback (registered on first paint — [`Shell::new`] has no
    /// window): keyboard shortcuts dispatch through the window focus chain, so
    /// with nothing focused they go dead. Initial focus lands on the composer
    /// and focus lost with no successor routes back there.
    focus_sub: Option<Subscription>,
    /// 1s heartbeat re-rendering the working indicator (elapsed + flavour word).
    _ticker: Task<()>,
    _state_observation: Subscription,
    /// Board frames repaint the sidebar's Agents section.
    _board_observation: Subscription,
    /// The board panel's one outward verb: "review this attempt" (gh#180).
    _board_events: Subscription,
    _composer_events: Subscription,
    /// "Fork from here", raised by the transcript row under the pointer.
    _transcript_events: Subscription,
}

impl Shell {
    pub fn new(state: Entity<AppState>, boot: EngineBootConfig, cx: &mut Context<Self>) -> Self {
        let observation = cx.observe(&state, |this: &mut Shell, state, cx| {
            this.on_state_changed(&state, cx);
            cx.notify();
        });
        let transcript = cx.new(|cx| Transcript::new(state.clone(), cx));
        let composer = cx.new(|cx| Composer::new(state.clone(), Some(boot.data_dir.clone()), cx));
        // The transcript's one outward verb: "fork this conversation here"
        // (gh#425). The menu it opens is shell chrome — a modal over the whole
        // window that ends by selecting another chat.
        let transcript_events =
            cx.subscribe(&transcript, |this: &mut Shell, _, event, cx| match event {
                transcript::TranscriptEvent::ForkAt { message_id } => {
                    this.open_fork(message_id.to_string(), cx)
                }
            });
        // Own-send re-engages the stick-to-bottom pin with a smooth scroll.
        let composer_events = cx.subscribe(&composer, {
            let transcript = transcript.clone();
            move |_this: &mut Shell, _, event: &ComposerEvent, cx| match event {
                ComposerEvent::Sent { .. } => {
                    transcript.update(cx, |t, cx| t.on_own_send(cx));
                }
            }
        });
        // Working-indicator heartbeat: notify once a second while a session is
        // live so elapsed time and the flavour word stay fresh.
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let alive = this.update(cx, |shell: &mut Shell, cx| {
                    let live = {
                        let s = shell.state.read(cx);
                        s.selected_chat
                            .as_deref()
                            .is_some_and(|id| s.indicator_for(id, Utc::now()) != Indicator::None)
                    };
                    if live {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        // The board panel is built up front, unlike the terminal and the diff
        // pane: its `WatchBoard` subscription is what feeds the sidebar's Agents
        // section (gh#103), which has to be there before anybody opens the dock.
        // Observing it repaints the sidebar on every board frame.
        let board = cx.new(|cx| BoardPanel::new(state.clone(), cx));
        let board_observation = cx.observe(&board, |_this: &mut Shell, _, cx| cx.notify());
        let board_events =
            cx.subscribe(
                &board,
                |this: &mut Shell, _, event: &BoardEvent, cx| match event {
                    BoardEvent::OpenReview { task_id, chat_id } => {
                        this.open_review(task_id.clone(), chat_id.clone(), cx)
                    }
                    // The popover's deep link (gh#490): the panel is the
                    // operational surface, the editor lives in Settings.
                    BoardEvent::OpenAutomations => {
                        this.board_open = false;
                        this.route = Route::Settings(SettingsSection::Automations);
                        this.nav
                            .push(NavEntry::Settings(SettingsSection::Automations));
                        cx.notify();
                    }
                },
            );
        let data_dir = boot.data_dir.clone();
        let settings = UiSettings::load(&data_dir);
        // Bind the customizable shortcuts from the persisted keymap.
        apply_keymap(cx, &settings.keymap);
        // Dev/testing knob: `COMET_OPEN_ROUTE=settings[/<section>]` boots
        // straight into a settings section — these pages have no deep link and
        // synthetic input can't reach them on headless compositors.
        let route = match std::env::var("COMET_OPEN_ROUTE").ok().as_deref() {
            Some("settings") | Some("settings/devices") => {
                Route::Settings(SettingsSection::Devices)
            }
            Some("settings/agents") => Route::Settings(SettingsSection::Agents),
            Some("settings/members") => Route::Settings(SettingsSection::Members),
            Some("settings/routing") => Route::Settings(SettingsSection::Routing),
            Some("settings/automations") => Route::Settings(SettingsSection::Automations),
            Some("settings/stats") => Route::Settings(SettingsSection::Stats),
            Some("settings/appearance") => Route::Settings(SettingsSection::Appearance),
            Some("settings/shortcuts") => Route::Settings(SettingsSection::Shortcuts),
            Some("settings/archived") => Route::Settings(SettingsSection::Archived),
            // `new` pins the new-chat canvas (suppresses boot auto-select).
            Some("new") => {
                state.update(cx, |s, _| s.auto_selected = true);
                Route::Chat
            }
            _ => Route::Chat,
        };
        // More capture knobs of the same kind: `COMET_OPEN_DIALOG=rename|delete`
        // opens that dialog for the first chat once chats land; `=model` pops
        // the combined harness/model menu once the shell is Ready;
        // `COMET_FORCE_GATE=signin|org|failed` renders that gate regardless of
        // real auth state (display-only — for styling passes).
        let debug_dialog = std::env::var("COMET_OPEN_DIALOG").ok();
        let debug_gate = match std::env::var("COMET_FORCE_GATE").ok().as_deref() {
            Some("signin") => Some(GatePhase::SignIn),
            Some("org") => Some(GatePhase::OrgGate),
            Some("failed") => Some(GatePhase::Failed(
                "Could not reach the comet engine on port 27901".into(),
            )),
            _ => None,
        };
        let nav = NavHistory::new(match &route {
            Route::Chat => NavEntry::Chat(String::new()),
            Route::Settings(section) => NavEntry::Settings(*section),
            Route::Review { task_id, chat_id } => NavEntry::Review {
                task_id: task_id.clone(),
                chat_id: chat_id.clone(),
            },
        });
        Self {
            state,
            transcript,
            composer,
            file_drag_active: false,
            terminal: None,
            changes: None,
            board,
            // Capture knob of the same family as the ones above
            // (`COMET_OPEN_BOARD=1`): boot with the right dock already showing
            // the board. The dock is a keypress away for a person, and that
            // keypress is exactly what a capture script cannot rely on — a
            // second app in front of this one swallows it, and the design pass
            // that has to photograph this panel then photographs an empty
            // window instead (gh#295).
            board_open: std::env::var("COMET_OPEN_BOARD").is_ok_and(|v| v == "1"),
            review: None,
            review_events: None,
            review_observation: None,
            route,
            nav,
            devices_page: None,
            members_page: None,
            routing_page: None,
            automations_page: None,
            stats_page: None,
            archived_page: None,
            shortcuts_page: None,
            accounts_page: None,
            appearance_page: None,
            shortcuts_sub: None,
            appearance_sub: None,
            chat_menu: None,
            rename_dialog: None,
            fork_dialog: None,
            fork_task: None,
            fork_models_task: None,
            delete_confirm: None,
            space_menu: None,
            rename_space_dialog: None,
            delete_space_confirm: None,
            add_space: None,
            space_last_chat: std::collections::HashMap::new(),
            tab_hover: None,
            tab_drag: None,
            space_drag: None,
            tabs_scroll: gpui::ScrollHandle::new(),
            tabs_scrolled_to: None,
            sidebar_scroll: gpui::ScrollHandle::new(),
            space_boot_applied: false,
            sound_prev: std::collections::HashMap::new(),
            user_menu_open: false,
            user_menu_dismissed_at: None,
            sidebar_notice: None,
            update_flow: UpdateFlow::Idle,
            update_task: None,
            update_dismissed: None,
            install: comet_update::detect_install(),
            org: None,
            mutate_task: None,
            new_session_task: None,
            last_new_session_action: None,
            new_session_intent: commands::load_intent(&data_dir),
            new_session_intent_attempted: false,
            new_session_intent_confirmed: false,
            new_session_chooser: None,
            focus_composer_next_render: false,
            auth_task: None,
            boot,
            data_dir,
            settings,
            panels: SessionPanels::default(),
            active_chat: String::new(),
            slug_sweep_seen: None,
            slug_task: None,
            revealed_chat: None,
            debug_dialog,
            debug_gate,
            sidebar_tween: None,
            right_tween: None,
            terminal_tween: None,
            fullscreen: None,
            titlebar_tween: None,
            titlebar_should_move: false,
            terminal_tween_task: None,
            terminal_drag_anchor: None,
            reduced_motion: false,
            motion_active: std::cell::Cell::new(false),
            splash: SplashPhase::Visible,
            splash_task: None,
            save_task: None,
            focus_sub: None,
            _ticker: ticker,
            _state_observation: observation,
            _board_observation: board_observation,
            _board_events: board_events,
            _composer_events: composer_events,
            _transcript_events: transcript_events,
        }
    }

    // ---- splash ----

    fn on_state_changed(&mut self, state: &Entity<AppState>, cx: &mut Context<Self>) {
        self.reconcile_new_session(cx);
        if self.new_session_intent.is_some()
            && !self.new_session_intent_attempted
            && state.read(cx).engine().is_some()
        {
            self.submit_new_session_intent(cx);
        }
        // Capture knob: the add-space palette needs only the device registry.
        // `add-space-error` opens it already carrying a failed clone (gh#317) —
        // the footer's one line is otherwise reachable only with a real board
        // host, a real App grant and a repo that really cannot be cloned.
        let wants_error = self.debug_dialog.as_deref() == Some("add-space-error");
        if (self.debug_dialog.as_deref() == Some("add-space") || wants_error)
            && !state.read(cx).devices.is_empty()
        {
            self.debug_dialog = None;
            self.open_add_space(cx);
            if wants_error {
                self.seed_add_space_error(cx);
            }
        }
        // Capture knob: pop the requested dialog once chats have landed.
        if let Some(which) = self.debug_dialog.clone()
            && let Some(first) = state.read(cx).chats.first().map(|c| c.id.clone())
        {
            self.debug_dialog = None;
            match which.as_str() {
                "rename" => self.open_rename_chat(first, cx),
                "delete" => {
                    self.delete_confirm = Some(first);
                }
                _ => {}
            }
        }
        // Session chimes (herdr semantics, `sound::sound_for_transition`): a
        // question rings whenever a session flips to AwaitingInput, a
        // completion rings on the Working→Idle edge — for ANY session on any
        // device. A row's first appearance only seeds the baseline, so boot
        // (restored rows) and fresh sends stay silent.
        //
        // STALENESS-GATED like the dot (`effective_indicator`), for the same
        // reason: raw row statuses include the past. A dead turn's Working row
        // (host killed mid-run, Idle write lost to a wedged room) seeded
        // prev=Working here, and the moment the old Idle finally synced in —
        // typically piggybacked on the round-trip of a fresh send — the chime
        // heard a phantom Working→Idle and rang "done" on send (user report
        // 2026-07-31). The dot never showed that ghost; the chime must judge
        // by the identical clock.
        {
            let now = Utc::now();
            let sessions: Vec<(String, comet_proto::SessionStatus)> = state
                .read(cx)
                .sessions
                .iter()
                .map(|s| {
                    use comet_proto::view::Indicator;
                    let status = match comet_proto::view::effective_indicator(Some(s), now) {
                        Indicator::Working => comet_proto::SessionStatus::Working,
                        Indicator::AwaitingInput => comet_proto::SessionStatus::AwaitingInput,
                        Indicator::Errored => comet_proto::SessionStatus::Errored,
                        Indicator::None => comet_proto::SessionStatus::Idle,
                    };
                    (s.chat_id.clone(), status)
                })
                .collect();
            for (chat_id, status) in sessions {
                let prev = self.sound_prev.insert(chat_id, status);
                if let Some(prev) = prev
                    && self.settings.sound_enabled
                    && let Some(sound) = crate::sound::sound_for_transition(prev, status)
                {
                    crate::sound::play(sound);
                }
            }
        }
        // Repo-first space rows (gh#124): re-ask the hosts for `space → repo`
        // links whenever the space/device MEMBERSHIP changes (never on
        // heartbeats — the fingerprint is ids only). Engine-gated so the first
        // real frame after connect still sweeps.
        if state.read(cx).engine().is_some() {
            let fingerprint: Vec<String> = {
                let s = state.read(cx);
                s.spaces
                    .iter()
                    .map(|space| space.id.clone())
                    .chain(s.devices.iter().map(|d| d.id.clone()))
                    .collect()
            };
            if self.slug_sweep_seen.as_ref() != Some(&fingerprint) {
                self.slug_sweep_seen = Some(fingerprint);
                self.refresh_space_slugs(cx);
            }
        }
        // Selecting a chat reveals it: the sidebar expands the chat's space so
        // the selection is never held by a row that is not on screen. Edge-
        // triggered — collapsing the space afterwards sticks until the
        // selection moves again. A chat opened from Active reveals its space
        // the same way, and the shelf below says where its row went (gh#138) —
        // one gesture, one outcome, whichever surface it came from.
        {
            let selected_chat = state.read(cx).selected_chat.clone();
            if selected_chat != self.revealed_chat {
                self.revealed_chat = selected_chat;
                if let Some(space) = state
                    .read(cx)
                    .selected_chat_row()
                    .and_then(|c| c.space_id.clone())
                {
                    self.expand_space(&space, cx);
                }
            }
        }
        // Boot: restore the last selected space once the first spaces frame
        // lands (a still-existing row wins over the auto-selected first one;
        // the boot-auto-selected chat's own space wins over both — selecting a
        // chat implies its space, which `select_chat` already applied).
        if !self.space_boot_applied && !state.read(cx).spaces.is_empty() {
            self.space_boot_applied = true;
            if state.read(cx).selected_chat.is_none()
                && let Some(last) = self.settings.last_space_id.clone()
                && state.read(cx).space_row(&last).is_some()
            {
                state.update(cx, |s, cx| s.select_space(Some(last), cx));
            }
        }
        // Track the per-space last chat + persist the selected space.
        {
            let (selected_space, selected_chat, chat_space) = {
                let s = state.read(cx);
                let chat_space = s.selected_chat_row().and_then(|c| c.space_id.clone());
                (
                    s.selected_space.clone(),
                    s.selected_chat.clone(),
                    chat_space,
                )
            };
            if let (Some(space), Some(chat)) = (chat_space, selected_chat) {
                self.space_last_chat.insert(space, chat);
            }
            if selected_space != self.settings.last_space_id && selected_space.is_some() {
                self.settings.last_space_id = selected_space;
                self.schedule_save(cx);
            }
        }
        // Chat switch: restore THAT chat's panel state (per-session open flags;
        // snap, no tween — the panels belong to the destination chat).
        let selected = state.read(cx).selected_chat.clone().unwrap_or_default();
        if selected != self.active_chat {
            self.active_chat = selected;
            // Route history: a chat switch is a navigation. The very first
            // selection off the untouched boot canvas REPLACES that entry —
            // comet's `/` route redirected into the last-used chat, leaving no
            // dead Back target. Walking history lands here too, but the
            // destination already equals `current()`, so the push dedups.
            if matches!(self.route, Route::Chat) {
                let entry = NavEntry::Chat(self.active_chat.clone());
                if self.nav.len() == 1 && *self.nav.current() == NavEntry::Chat(String::new()) {
                    self.nav.replace(entry);
                } else {
                    self.nav.push(entry);
                }
            }
            self.right_tween = None;
            self.terminal_tween = None;
            let panels = self.panels.get(&self.panel_key(cx));
            if let Some(panel) = self.terminal.clone() {
                panel.update(cx, |panel, cx| panel.set_open(panels.terminal_open, cx));
            }
            if panels.changes_open {
                let changes = self.changes_pane(cx);
                changes.update(cx, |changes, cx| changes.ensure_watch(cx));
            }
        }
        match state.read(cx).connection {
            ConnectionStatus::Ready => {
                if self.splash == SplashPhase::Visible {
                    self.splash = SplashPhase::FadingOut;
                    self.splash_task = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor()
                            .timer(SPLASH_OUT.total() + Duration::from_millis(30))
                            .await;
                        this.update(cx, |shell, cx| {
                            shell.splash = SplashPhase::Gone;
                            cx.notify();
                        })
                        .ok();
                    }));
                }
            }
            // Reveal the gate card immediately; the splash never returns mid-session.
            ConnectionStatus::Failed(_) => self.splash = SplashPhase::Gone,
            ConnectionStatus::Connecting => {}
        }
    }

    // ---- layout state ----

    fn sidebar_target(&self) -> f32 {
        if self.settings.sidebar_collapsed {
            0.0
        } else {
            self.settings.sidebar_width
        }
    }

    /// Does the selected space's folder have git? Owner-stamped and synced —
    /// gates the Changes pane, its toggle, and Cmd-B with zero RPCs.
    fn space_git_detected(&self, cx: &App) -> bool {
        self.state.read(cx).selected_space_git()
    }

    /// The current chat's changes-pane flag (per-session, in-memory), gated on
    /// the space having git at all: a stale per-chat open flag must not reopen
    /// the pane after switching into a non-git space.
    /// The per-session panel key. The new-chat canvas (no selection) keys per
    /// SPACE — one shared "" key made a canvas toggle read as global state
    /// (user report).
    fn panel_key(&self, cx: &App) -> String {
        if self.active_chat.is_empty() {
            let space = self
                .state
                .read(cx)
                .selected_space
                .clone()
                .unwrap_or_default();
            format!("space-canvas:{space}")
        } else {
            self.active_chat.clone()
        }
    }

    fn right_pane_open(&self, cx: &App) -> bool {
        self.panels.get(&self.panel_key(cx)).changes_open && self.space_git_detected(cx)
    }

    /// Whether the right dock holds anything at all: the changes pane or the
    /// board. Only one is ever open — toggling one closes the other — so this
    /// is the single open test the width target and the card margin use.
    fn right_slot_open(&self, cx: &App) -> bool {
        self.board_open || self.right_pane_open(cx)
    }

    /// The current chat's terminal flag (per-session, in-memory).
    fn terminal_open(&self, cx: &App) -> bool {
        self.panels.get(&self.panel_key(cx)).terminal_open
    }

    fn right_target(&self, cx: &App) -> f32 {
        if self.right_slot_open(cx) {
            self.settings.right_pane_width
        } else {
            0.0
        }
    }

    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        let from = self.sidebar_target();
        self.settings.sidebar_collapsed = !self.settings.sidebar_collapsed;
        self.sidebar_tween = Some(WidthTween::new(from, self.sidebar_target()));
        self.schedule_save(cx);
        cx.notify();
    }

    fn toggle_right_pane(&mut self, cx: &mut Context<Self>) {
        // No git in this space → no diff pane, Cmd-B goes dead.
        if !self.space_git_detected(cx) {
            return;
        }
        let from = self.right_target(cx);
        let key = self.panel_key(cx);
        let open = self.panels.toggle_changes(&key);
        if open {
            // The dock shows one thing: opening changes closes the board (the
            // changes flag keeps its own per-session life either way).
            self.board_open = false;
            // Lazy: the Changes entity (and its WatchCheckoutDiffs) exists only
            // once the pane has been opened.
            let changes = self.changes_pane(cx);
            changes.update(cx, |changes, cx| changes.ensure_watch(cx));
        }
        self.right_tween = Some(WidthTween::new(from, self.right_target(cx)));
        cx.notify();
    }

    fn changes_pane(&mut self, cx: &mut Context<Self>) -> Entity<Changes> {
        if let Some(changes) = &self.changes {
            return changes.clone();
        }
        let changes = cx.new(|cx| Changes::new(self.state.clone(), cx));
        self.changes = Some(changes.clone());
        changes
    }

    /// Cmd/Ctrl+Shift+B and the tab-strip button (§gh#70). The
    /// board is a global queue, so unlike the per-session terminal/changes
    /// flags this one lives on the shell. Width animates 200 ms in the shared
    /// right-dock slot, which shows one thing at a time: opening the board
    /// closes the changes pane.
    fn toggle_board(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let from = self.right_target(cx);
        self.board_open = !self.board_open;
        if self.board_open {
            // The changes flag flips off so the dock does not silently re-show
            // the diff pane the moment the board closes.
            let key = self.panel_key(cx);
            if self.panels.get(&key).changes_open {
                self.panels.toggle_changes(&key);
            }
            let panel = self.board.clone();
            panel.update(cx, |panel, cx| panel.set_open(true, cx));
            // Keyboard focus lands in the board so ↑↓/f// keys work with no
            // click — the same lazy-focus the terminal panel gets.
            window.focus(&panel.read(cx).focus_handle(), cx);
        } else {
            window.focus(&self.composer.focus_handle(cx), cx);
        }
        self.right_tween = Some(WidthTween::new(from, self.right_target(cx)));
        cx.notify();
    }

    /// The board toggle as the whole app means it — what `mod-shift-b` does and
    /// what the titlebar's checklist button does, in one place because the
    /// strip is drawn on the review route too now (gh#311) and the two must not
    /// be two answers to one gesture.
    ///
    /// Called directly rather than by dispatching [`ToggleBoard`] from the
    /// button: an action dispatched out of a click listener is routed through
    /// the focus chain, and a click on the titlebar can leave nothing focused —
    /// which is a button that silently does nothing (seen while photographing
    /// this issue).
    pub(super) fn toggle_board_from_route(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.route {
            Route::Chat => self.toggle_board(window, cx),
            // The board is where a review is opened from, so the same gesture
            // is the way back to it: leave the route, then open the dock on the
            // session the review was of.
            Route::Review { .. } => {
                self.close_review(cx);
                self.toggle_board(window, cx);
            }
            Route::Settings(_) => {}
        }
    }

    fn terminal_panel(&mut self, cx: &mut Context<Self>) -> Entity<TerminalPanel> {
        if let Some(terminal) = &self.terminal {
            return terminal.clone();
        }
        let terminal = cx.new(|cx| TerminalPanel::new(self.state.clone(), cx));
        self.terminal = Some(terminal.clone());
        terminal
    }

    fn terminal_target(&self, cx: &App) -> f32 {
        if self.terminal_open(cx) {
            self.settings.terminal_height
        } else {
            0.0
        }
    }

    /// Cmd/Ctrl+J and the header button (feature-inventory §1.10). Height
    /// animates 200 ms; closing detaches (PTYs stay alive), opening restores.
    /// The flag is per chat (comet `sessionPanels`).
    fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let from = self.terminal_target(cx);
        let key = self.panel_key(cx);
        let open = self.panels.toggle_terminal(&key);
        self.terminal_tween = Some(WidthTween::new(from, self.terminal_target(cx)));
        let panel = self.terminal_panel(cx);
        panel.update(cx, |panel, cx| panel.set_open(open, cx));
        if open {
            // Opening lands keyboard focus IN the shell — typing goes straight
            // to the prompt, no click needed (comet terminal-panel.tsx: the
            // visible+active effect calls `terminal.focus()` on every open).
            // The handle is focusable before the panel's first paint; once the
            // terminal body mounts with `track_focus` it receives the keys.
            window.focus(&panel.read(cx).focus_handle(), cx);
        } else {
            // Hiding the panel removes the (likely focused) terminal view;
            // with nothing focused, window key bindings stop dispatching, so
            // hand focus to the composer. (Cmd+J is a pure toggle — a second
            // press closes even while the terminal is focused, as in comet's
            // `useHotkey(toggleShortcut, ... setOpenScoped(!open))`.)
            window.focus(&self.composer.focus_handle(cx), cx);
        }
        self.terminal_tween_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(RESIZE.total().mul_f32(motion::speed_scale()) + Duration::from_millis(30))
                .await;
            this.update(cx, |shell, cx| {
                shell.terminal_tween = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn on_terminal_drag(
        &mut self,
        event: &gpui::DragMoveEvent<TerminalResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((anchor_y, anchor_h)) = self.terminal_drag_anchor else {
            return;
        };
        let dy = anchor_y - f32::from(event.event.position.y);
        let viewport_h = f32::from(window.viewport_size().height);
        self.settings.terminal_height = clamp_terminal_height(anchor_h + dy, viewport_h);
        self.terminal_tween = None; // live drag tracks the pointer
        self.schedule_save(cx);
        cx.notify();
    }

    fn on_sidebar_drag(
        &mut self,
        event: &gpui::DragMoveEvent<SidebarResize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let x = f32::from(event.event.position.x);
        self.settings.sidebar_width = x.clamp(SIDEBAR_MIN, SIDEBAR_MAX);
        self.settings.sidebar_collapsed = false;
        self.sidebar_tween = None; // live drag tracks the pointer directly
        self.schedule_save(cx);
        cx.notify();
    }

    fn on_right_pane_drag(
        &mut self,
        event: &gpui::DragMoveEvent<RightPaneResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let viewport = f32::from(window.viewport_size().width);
        let width = viewport - f32::from(event.event.position.x);
        // comet caps the pane at 52% of the window on top of the absolute range.
        let max = RIGHT_PANE_MAX.min(viewport * 0.52);
        self.settings.right_pane_width = width.clamp(RIGHT_PANE_MIN, max.max(RIGHT_PANE_MIN));
        self.right_tween = None;
        self.schedule_save(cx);
        cx.notify();
    }

    /// The review route's seam (gh#180). Measured from the RIGHT edge like the
    /// changes pane's, because the session column sits in the dock's slot
    /// (gh#276) and it is that card's left edge you drag.
    fn on_review_session_drag(
        &mut self,
        event: &gpui::DragMoveEvent<ReviewSessionResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let viewport = f32::from(window.viewport_size().width);
        let width = viewport - f32::from(event.event.position.x);
        self.settings.review_session_width = width.clamp(REVIEW_SESSION_MIN, REVIEW_SESSION_MAX);
        self.schedule_save(cx);
        cx.notify();
    }

    /// Debounced settings write: waits [`SAVE_DEBOUNCE_MS`], then persists the
    /// latest snapshot on the background executor. Re-scheduling drops (cancels)
    /// the previous timer.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        let dir = self.data_dir.clone();
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(SAVE_DEBOUNCE_MS))
                .await;
            let Ok(snapshot) = this.update(cx, |shell, _| shell.settings.clone()) else {
                return;
            };
            cx.background_executor()
                .spawn(async move {
                    if let Err(err) = snapshot.save(&dir) {
                        tracing::warn!(error = %err, "failed to persist ui settings");
                    }
                })
                .await;
        }));
    }

    fn retry_engine(&mut self, cx: &mut Context<Self>) {
        AppState::bootstrap(self.state.clone(), self.boot.clone(), cx);
    }

    // ---- routes / settings ----

    fn open_settings(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        self.route = Route::Settings(section);
        self.nav.push(NavEntry::Settings(section));
        self.user_menu_open = false;
        self.chat_menu = None;
        cx.notify();
    }

    fn close_settings(&mut self, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        self.nav.push(NavEntry::Chat(self.active_chat.clone()));
        cx.notify();
    }

    /// Open one attempt's review (gh#180) — the inverted route.
    ///
    /// The chat comes from the board row rather than from whatever is selected,
    /// and is *selected* here so the column beside the review is the session
    /// that authored it. An attempt whose chat is gone opens the review alone,
    /// which is the case the claims were recorded on the attempt for.
    ///
    /// The board dock shuts on the way through: the review is the destination,
    /// and leaving the queue open over it would put two full-height surfaces on
    /// one screen with the reading squeezed between them.
    fn open_review(&mut self, task_id: String, chat_id: Option<String>, cx: &mut Context<Self>) {
        if let Some(chat) = chat_id.clone()
            && self.state.read(cx).selected_chat.as_deref() != Some(chat.as_str())
        {
            self.state.update(cx, |s, cx| s.select_chat(Some(chat), cx));
        }
        // A different task is a different panel: replacing it (rather than
        // re-pointing one) is what makes an in-flight reply for the review you
        // just left have nowhere to land.
        self.ensure_review(&task_id, chat_id.as_deref(), cx);
        self.board_open = false;
        self.route = Route::Review {
            task_id: task_id.clone(),
            chat_id: chat_id.clone(),
        };
        self.nav.push(NavEntry::Review { task_id, chat_id });
        self.user_menu_open = false;
        self.chat_menu = None;
        cx.notify();
    }

    /// Build the review card for `task_id` if the live one is not already it.
    ///
    /// A different task is a different panel — replacing it rather than
    /// re-pointing one is what makes an in-flight reply for the review you just
    /// left have nowhere to land — and the subscription is replaced with it, so
    /// a dropped card cannot still be asking for the diff.
    fn ensure_review(&mut self, task_id: &str, chat_id: Option<&str>, cx: &mut Context<Self>) {
        if self
            .review
            .as_ref()
            .is_some_and(|panel| panel.read(cx).task_id() == task_id)
        {
            return;
        }
        let state = self.state.clone();
        let id = task_id.to_string();
        let chat = chat_id.map(str::to_string);
        let panel = cx.new(|cx| ReviewPanel::new(state, id, chat, None, cx));
        self.review_events = Some(cx.subscribe(
            &panel,
            |this: &mut Shell, _, event: &ReviewEvent, cx| match event {
                ReviewEvent::ReadTheDiff { chat_id } => this.read_the_diff(chat_id.clone(), cx),
            },
        ));
        // The delivery preview is drawn by the SHELL, in the session column
        // (gh#276), out of the panel's own state — so the shell has to repaint
        // when that state moves, or the preview would stop following the
        // sentence being typed the moment it left the card.
        self.review_observation = Some(cx.observe(&panel, |_: &mut Shell, _, cx| cx.notify()));
        self.review = Some(panel);
    }

    /// The `Read the diff` chip (§gh#238): leave the review for the diff it is
    /// about.
    ///
    /// The review card refuses to draw a diff and the [`Changes`] pane is
    /// chat-route chrome, so "one click away" is one route change and one dock:
    /// select the chat that owns the checkout, drop back to the chat route, and
    /// open the pane if it is not already open. Idempotent on the pane — a
    /// second click on a session whose pane is already open would otherwise
    /// close the very thing the chip was asking for.
    ///
    /// `active_chat` is set here rather than waited for: the panel key the
    /// pane's open flag lives under is derived from it, and the observation
    /// that normally sets it has not run yet inside this update.
    fn read_the_diff(&mut self, chat_id: String, cx: &mut Context<Self>) {
        if self.state.read(cx).selected_chat.as_deref() != Some(chat_id.as_str()) {
            self.state
                .update(cx, |s, cx| s.select_chat(Some(chat_id.clone()), cx));
        }
        self.active_chat = chat_id;
        self.close_review(cx);
        if !self.right_pane_open(cx) {
            self.toggle_right_pane(cx);
        }
    }

    /// Leave a review by selecting another session's tab (gh#311).
    ///
    /// `active_chat` is set here rather than waited for, exactly as
    /// [`Self::read_the_diff`] does: [`Self::close_review`] records the route
    /// history entry off it, and the observation that normally updates it has
    /// not run yet inside this update — so without this the Back target would
    /// be the session you just left rather than the one you just picked.
    pub(super) fn leave_review_for(&mut self, chat_id: String, cx: &mut Context<Self>) {
        if self.state.read(cx).selected_chat.as_deref() != Some(chat_id.as_str()) {
            self.state
                .update(cx, |s, cx| s.select_chat(Some(chat_id.clone()), cx));
        }
        self.active_chat = chat_id;
        self.close_review(cx);
    }

    /// Leave a review for the session it was about. The panel is dropped: a
    /// review is a snapshot of a diff and a run journal, and the next visit
    /// should re-read both rather than show what the box said last time.
    pub(super) fn close_review(&mut self, cx: &mut Context<Self>) {
        self.review = None;
        self.review_events = None;
        self.review_observation = None;
        self.route = Route::Chat;
        self.nav.push(NavEntry::Chat(self.active_chat.clone()));
        cx.notify();
    }

    /// Whether something open on top of the page owns the keyboard: a menu, a
    /// dialog, the add-space palette, or the board dock.
    ///
    /// `esc` belongs to the innermost thing that is open, and a route is the
    /// outermost thing there is — so the route's own escape hatch (gh#311) asks
    /// this first. The surfaces that handle the key themselves stop its
    /// propagation and never reach the root listener; this covers the ones that
    /// do not, where closing the route out from under an open dialog would be
    /// the wrong answer to one keypress.
    fn overlay_open(&self) -> bool {
        self.chat_menu.is_some()
            || self.rename_dialog.is_some()
            || self.fork_dialog.is_some()
            || self.delete_confirm.is_some()
            || self.space_menu.is_some()
            || self.rename_space_dialog.is_some()
            || self.delete_space_confirm.is_some()
            || self.add_space.is_some()
            || self.new_session_chooser.is_some()
            || self.user_menu_open
            || self.board_open
    }

    /// Surfaces that must block application commands. The board is a docked,
    /// workspace-scoped screen, not a modal, so it deliberately is not here.
    fn command_modal_open(&self) -> bool {
        self.chat_menu.is_some()
            || self.rename_dialog.is_some()
            || self.fork_dialog.is_some()
            || self.delete_confirm.is_some()
            || self.space_menu.is_some()
            || self.rename_space_dialog.is_some()
            || self.delete_space_confirm.is_some()
            || self.add_space.is_some()
            || self.new_session_chooser.is_some()
            || self.user_menu_open
    }

    // ---- back/forward (route history) ----

    fn navigate_back(&mut self, cx: &mut Context<Self>) {
        if let Some(entry) = self.nav.back() {
            self.apply_nav(entry, cx);
        }
    }

    fn navigate_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(entry) = self.nav.forward() {
            self.apply_nav(entry, cx);
        }
    }

    /// Land on a history entry WITHOUT recording a new one: the stack already
    /// points at `entry` (back/forward moved the index); the selection change
    /// this triggers dedups against `current()` in [`Self::on_state_changed`].
    fn apply_nav(&mut self, entry: NavEntry, cx: &mut Context<Self>) {
        match entry {
            NavEntry::Chat(chat_id) => {
                self.route = Route::Chat;
                let target = (!chat_id.is_empty()).then_some(chat_id);
                if self.state.read(cx).selected_chat != target {
                    self.state.update(cx, |s, cx| s.select_chat(target, cx));
                }
            }
            NavEntry::Settings(section) => {
                self.route = Route::Settings(section);
            }
            NavEntry::Review { task_id, chat_id } => {
                if let Some(chat) = chat_id.clone()
                    && self.state.read(cx).selected_chat.as_deref() != Some(chat.as_str())
                {
                    self.state.update(cx, |s, cx| s.select_chat(Some(chat), cx));
                }
                self.ensure_review(&task_id, chat_id.as_deref(), cx);
                self.route = Route::Review { task_id, chat_id };
            }
        }
        self.user_menu_open = false;
        self.chat_menu = None;
        cx.notify();
    }

    /// Lazily create the entity for a settings section and return it renderable.
    fn settings_outlet(&mut self, section: SettingsSection, cx: &mut Context<Self>) -> AnyElement {
        match section {
            SettingsSection::Devices => {
                if self.devices_page.is_none() {
                    let state = self.state.clone();
                    self.devices_page = Some(cx.new(|cx| DevicesPage::new(state, cx)));
                }
                match &self.devices_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Agents => {
                if self.accounts_page.is_none() {
                    let state = self.state.clone();
                    self.accounts_page = Some(cx.new(|cx| AccountsPage::new(state, cx)));
                }
                match &self.accounts_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Members => {
                if self.members_page.is_none() {
                    let state = self.state.clone();
                    self.members_page = Some(cx.new(|cx| MembersPage::new(state, cx)));
                }
                match &self.members_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Routing => {
                if self.routing_page.is_none() {
                    let state = self.state.clone();
                    self.routing_page = Some(cx.new(|cx| RoutingPage::new(state, cx)));
                }
                match &self.routing_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Automations => {
                if self.automations_page.is_none() {
                    let state = self.state.clone();
                    self.automations_page = Some(cx.new(|cx| AutomationsPage::new(state, cx)));
                }
                match &self.automations_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Stats => {
                if self.stats_page.is_none() {
                    let state = self.state.clone();
                    self.stats_page = Some(cx.new(|cx| StatsPage::new(state, cx)));
                }
                match &self.stats_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Appearance => {
                if self.appearance_page.is_none() {
                    let state = self.state.clone();
                    let choice = self.settings.theme;
                    let page = cx.new(|_| AppearancePage::new(state, choice));
                    // Apply the new variant globally + persist whenever the
                    // page changes it — the flip is live (every render reads
                    // `Theme::of(cx)` fresh).
                    self.appearance_sub = Some(cx.subscribe(
                        &page,
                        |this: &mut Shell, _, event: &AppearanceEvent, cx| {
                            let AppearanceEvent::ThemeChanged(choice) = event;
                            this.settings.theme = *choice;
                            cx.set_global(Theme::for_choice(*choice));
                            this.schedule_save(cx);
                            cx.notify();
                        },
                    ));
                    self.appearance_page = Some(page);
                }
                match &self.appearance_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Shortcuts => {
                if self.shortcuts_page.is_none() {
                    let state = self.state.clone();
                    let keymap = self.settings.keymap.clone();
                    let page = cx.new(|cx| ShortcutsPage::new(state, keymap, cx));
                    // Persist + re-apply the keymap whenever the page changes it.
                    self.shortcuts_sub = Some(cx.subscribe(
                        &page,
                        |this: &mut Shell, _, event: &ShortcutsEvent, cx| {
                            let ShortcutsEvent::Changed(keymap) = event;
                            this.settings.keymap = keymap.clone();
                            apply_keymap(cx, keymap);
                            this.schedule_save(cx);
                            cx.notify();
                        },
                    ));
                    self.shortcuts_page = Some(page);
                }
                match &self.shortcuts_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
            SettingsSection::Archived => {
                if self.archived_page.is_none() {
                    let state = self.state.clone();
                    self.archived_page = Some(cx.new(|cx| ArchivedPage::new(state, cx)));
                }
                match &self.archived_page {
                    Some(page) => page.clone().into_any_element(),
                    None => Empty.into_any_element(),
                }
            }
        }
    }

    // ---- sidebar mutations ----

    fn handle_new_session_chooser_key(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        let Some(selected) = self.new_session_chooser else {
            return false;
        };
        let count = self.state.read(cx).spaces.len();
        match key {
            "up" if count > 0 => {
                self.new_session_chooser = Some(commands::step_chooser(selected, count, -1));
                cx.notify();
            }
            "down" if count > 0 => {
                self.new_session_chooser = Some(commands::step_chooser(selected, count, 1));
                cx.notify();
            }
            "enter" if count > 0 => {
                let space = self.state.read(cx).spaces[selected.min(count - 1)]
                    .id
                    .clone();
                self.new_session(Some(space), cx);
            }
            "enter" => {
                self.new_session_chooser = None;
                self.open_add_space(cx);
            }
            "escape" => {
                self.new_session_chooser = None;
                cx.notify();
            }
            _ => return false,
        }
        true
    }

    /// Fire a Mutate op; failures surface in the sidebar notice strip.
    fn mutate(&mut self, params: serde_json::Value, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.sidebar_notice = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        self.mutate_task = Some(cx.spawn(async move |this, cx| {
            if let Err(err) = engine.client().call(methods::MUTATE, params).await {
                this.update(cx, |shell, cx| {
                    shell.sidebar_notice = Some(format!("{err}").into());
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    /// Execute the typed New Session command. Menu items, ⌘T, and chooser rows
    /// all enter here; no input handler owns session-creation logic.
    fn new_session(&mut self, requested_space: Option<String>, cx: &mut Context<Self>) {
        if !commands::NEW_SESSION.available(
            requested_space.is_none() && self.command_modal_open(),
            self.new_session_task.is_some(),
        ) {
            return;
        }
        if requested_space.is_none() {
            let now = Instant::now();
            if self
                .last_new_session_action
                .is_some_and(|last| now.duration_since(last) < Duration::from_millis(250))
            {
                return;
            }
            self.last_new_session_action = Some(now);
        }
        if self.new_session_intent.is_some() {
            // Resolve the durable idempotency key before any replacement. A
            // delayed WATCH may still reveal a commit whose reply was lost.
            self.new_session_intent_attempted = false;
            self.submit_new_session_intent(cx);
            return;
        }
        let space_id = requested_space.or_else(|| {
            commands::current_space(
                self.state.read(cx),
                matches!(self.route, Route::Chat) || self.board_open,
            )
        });
        let Some(space_id) = space_id else {
            self.new_session_chooser = Some(0);
            cx.notify();
            return;
        };
        let intent = NewSessionIntent {
            chat_id: uuid::Uuid::new_v4().to_string(),
            space_id,
        };
        if let Err(err) = commands::save_intent(&self.data_dir, &intent) {
            self.sidebar_notice = Some(format!(
                "New Session failed: couldn't save its recovery record ({err}). Check disk access and try again."
            ).into());
            cx.notify();
            return;
        }
        self.new_session_intent = Some(intent);
        self.new_session_intent_attempted = false;
        self.new_session_intent_confirmed = false;
        self.new_session_chooser = None;
        self.submit_new_session_intent(cx);
    }

    fn submit_new_session_intent(&mut self, cx: &mut Context<Self>) {
        if self.new_session_task.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.sidebar_notice = Some(
                "New Session failed: engine not connected. Try again when Comet reconnects.".into(),
            );
            cx.notify();
            return;
        };
        let Some(intent) = self.new_session_intent.clone() else {
            return;
        };
        self.new_session_intent_attempted = true;
        let params = serde_json::json!({
            "op": "createChat", "chatId": intent.chat_id, "spaceId": intent.space_id,
        });
        self.new_session_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::MUTATE, params).await;
            let absence_proven = if result.as_ref().is_err_and(|err| {
                !matches!(err, comet_rpc::RpcError::Closed | comet_rpc::RpcError::Transport(_))
            }) {
                engine
                    .client()
                    .call(
                        methods::MUTATE,
                        serde_json::json!({
                            "op": "confirmChatAbsent", "chatId": intent.chat_id,
                        }),
                    )
                    .await
                    .is_ok()
            } else {
                false
            };
            this.update(cx, |shell, cx| {
                shell.new_session_task = None;
                if result.is_ok() {
                    shell.new_session_intent_confirmed = true;
                } else if let Err(err) = result {
                    // Keep the stable intent. A retry (or a recreated Shell)
                    // reuses its UUID, so commit-before-response-loss cannot
                    // strand a second empty session.
                    shell.new_session_intent_confirmed = false;
                    if absence_proven {
                        match commands::clear_intent(&shell.data_dir) {
                            Ok(()) => {
                                shell.new_session_intent = None;
                                shell.new_session_intent_attempted = false;
                                shell.new_session_chooser = Some(0);
                                shell.sidebar_notice = Some(format!(
                                    "New Session was refused: {err}. Choose a workspace to try again."
                                ).into());
                            }
                            Err(clear_err) => {
                                shell.sidebar_notice = Some(format!(
                                    "New Session was refused, but its recovery record could not be cleared ({clear_err}). Fix disk access and retry."
                                ).into());
                            }
                        }
                    } else {
                        shell.sidebar_notice = Some(format!(
                            "Couldn't durably confirm New Session: {err}. Press New Session again to safely retry the same session."
                        ).into());
                    }
                }
                shell.reconcile_new_session(cx);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Selection and recovery acknowledgement require both sides of the
    /// transaction: durable RPC success and the exact WATCH_CHATS row.
    fn reconcile_new_session(&mut self, cx: &mut Context<Self>) {
        let Some(intent) = self.new_session_intent.clone() else {
            return;
        };
        if !self.new_session_intent_confirmed {
            return;
        }
        if !commands::intent_acknowledged(
            &intent,
            self.state.read(cx),
            self.new_session_intent_confirmed,
        ) {
            return;
        }
        if let Err(err) = commands::clear_intent(&self.data_dir) {
            self.sidebar_notice = Some(format!(
                "Session was created, but its recovery record could not be cleared ({err}). Fix disk access and retry."
            ).into());
            return;
        }
        self.new_session_intent = None;
        self.new_session_intent_attempted = false;
        self.new_session_intent_confirmed = false;
        self.new_session_task = None;
        if matches!(self.route, Route::Review { .. }) {
            self.close_review(cx);
        } else {
            self.route = Route::Chat;
        }
        self.state
            .update(cx, |state, cx| state.select_chat(Some(intent.chat_id), cx));
        self.focus_composer_next_render = true;
    }

    fn open_rename_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.chat_menu = None;
        let current = self
            .state
            .read(cx)
            .chats
            .iter()
            .find(|c| c.id == chat_id)
            .and_then(|c| c.title.clone())
            .unwrap_or_default();
        let input = cx.new(|cx| ComposerInput::new("Session title", cx));
        input.update(cx, |input, cx| input.set_text(current, cx));
        let events = cx.subscribe(&input, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_rename_chat(cx);
            }
        });
        self.rename_dialog = Some(RenameChatDialog {
            chat_id,
            input,
            focus_pending: true,
            _events: events,
        });
        cx.notify();
    }

    fn submit_rename_chat(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.rename_dialog.take() else {
            return;
        };
        let title = dialog.input.read(cx).text().trim().to_string();
        if !title.is_empty() {
            self.mutate(
                serde_json::json!({ "op": "renameChat", "chatId": dialog.chat_id, "title": title }),
                cx,
            );
        }
        cx.notify();
    }

    fn archive_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.chat_menu = None;
        self.mutate(
            serde_json::json!({ "op": "setChatArchived", "chatId": chat_id, "archived": true }),
            cx,
        );
        cx.notify();
    }

    /// Pin a chat as the board's orchestrator, or unpin whatever is (gh#104).
    ///
    /// `[defaults] orchestrator_chat` on the board's `routing.toml`, written
    /// through `WriteBoardConfig` — the same validated, backed-up path the
    /// routing settings page uses, rather than a second way to edit that file.
    /// `None` removes the key: the notices stop and the chat goes back to being
    /// an ordinary chat, which is the whole of the kill switch.
    ///
    /// Nothing is applied optimistically. The board republishes the pin as the
    /// write lands, so the glyph appearing *is* the box agreeing; a refusal
    /// leaves the list as it was and says why.
    fn set_orchestrator(&mut self, chat_id: Option<String>, cx: &mut Context<Self>) {
        self.chat_menu = None;
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.sidebar_notice = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        let (devices, local) = {
            let state = self.state.read(cx);
            (state.devices.clone(), state.local_device_id.clone())
        };
        // The board is on one device and this may not be it. Sweep the same
        // candidates its panel does; the first host that accepts the write is
        // the one hosting the board.
        let candidates = comet_proto::view::board::host_candidates(&devices, local.as_deref());
        self.mutate_task = Some(cx.spawn(async move |this, cx| {
            let mut last: Option<String> = None;
            for candidate in candidates {
                let mut params = serde_json::json!({
                    "op": "default", "key": "orchestrator_chat"
                });
                if let Some(object) = params.as_object_mut() {
                    if let Some(id) = &chat_id {
                        object.insert("value".into(), serde_json::json!(id));
                    }
                    if let Some(host) = candidate.as_deref() {
                        object.insert("targetDeviceId".into(), serde_json::json!(host));
                    }
                }
                match engine
                    .client()
                    .call(methods::WRITE_BOARD_CONFIG, params)
                    .await
                {
                    // The pin arrives back on the watch stream, not from here.
                    Ok(_) => return,
                    Err(err) => last = Some(err.to_string()),
                }
            }
            this.update(cx, |shell, cx| {
                shell.sidebar_notice = Some(
                    match last {
                        Some(err) => format!("No device here hosts a board ({err})"),
                        None => "No device here hosts a board".to_string(),
                    }
                    .into(),
                );
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn delete_chat(&mut self, chat_id: String, cx: &mut Context<Self>) {
        self.delete_confirm = None;
        if self.state.read(cx).selected_chat.as_deref() == Some(chat_id.as_str()) {
            self.state.update(cx, |s, cx| s.select_chat(None, cx));
        }
        self.composer
            .update(cx, |composer, cx| composer.purge_chat(&chat_id, cx));
        self.mutate(
            serde_json::json!({ "op": "deleteChat", "chatId": chat_id }),
            cx,
        );
        cx.notify();
    }

    fn sign_out(&mut self, cx: &mut Context<Self>) {
        self.user_menu_open = false;
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.auth_task = Some(cx.spawn(async move |this, cx| {
            if let Err(err) = engine
                .client()
                .call(methods::SIGN_OUT, serde_json::json!({}))
                .await
            {
                this.update(cx, |shell, cx| {
                    shell.sidebar_notice = Some(format!("Sign out failed: {err}").into());
                    cx.notify();
                })
                .ok();
            }
        }));
        cx.notify();
    }

    fn start_sign_in(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.auth_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::SIGN_IN, serde_json::json!({}))
                .await;
            this.update(cx, |shell, cx| match result {
                Ok(value) => {
                    if let Some(url) = value.get("url").and_then(|u| u.as_str()) {
                        cx.open_url(url);
                    }
                }
                Err(err) => {
                    shell.sidebar_notice = Some(format!("Sign in failed: {err}").into());
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    // ---- org gate ----

    fn ensure_org_ui(&mut self, cx: &mut Context<Self>) {
        if self.org.is_some() {
            return;
        }
        let name_input = cx.new(|cx| ComposerInput::new("Workspace name", cx));
        let events = cx.subscribe(&name_input, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.create_org(cx);
            }
        });
        let invite_input = cx.new(|cx| ComposerInput::new("Invitation code", cx));
        let invite_events = cx.subscribe(&invite_input, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.accept_invite(cx);
            }
        });
        self.org = Some(OrgGateUi {
            name_input,
            invite_input,
            orgs: Loadable::Idle,
            submitting: false,
            error: None,
            task: None,
            _events: events,
            _invite_events: invite_events,
        });
        self.load_orgs(cx);
    }

    fn load_orgs(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(org) = self.org.as_mut() else { return };
        org.orgs = Loadable::Loading;
        org.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_ORGS, serde_json::json!({}))
                .await;
            this.update(cx, |shell, cx| {
                if let Some(org) = shell.org.as_mut() {
                    org.orgs = match result {
                        Ok(value) => Loadable::Ready(sort_memberships(parse_orgs(&value))),
                        Err(err) => Loadable::Error(err.to_string()),
                    };
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn create_org(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(org) = self.org.as_mut() else { return };
        if org.submitting {
            return;
        }
        let name = org.name_input.read(cx).text().trim().to_string();
        if !org_name_valid(&name) {
            org.error = Some("Enter a workspace name".into());
            cx.notify();
            return;
        }
        org.submitting = true;
        org.error = None;
        org.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::CREATE_ORG, serde_json::json!({ "name": name }))
                .await;
            this.update(cx, |shell, cx| {
                if let Some(org) = shell.org.as_mut() {
                    org.submitting = false;
                    if let Err(err) = result {
                        org.error = Some(format!("{err}").into());
                    }
                    // Success: the AuthStatus stream flips to SignedIn and the
                    // gate falls away on its own.
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Redeem a pasted invitation code (gh#76). On success the engine scopes
    /// the session to the workspace that invited us, the AuthStatus stream
    /// flips to SignedIn, and this gate falls away — same landing as picking a
    /// membership.
    fn accept_invite(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(org) = self.org.as_mut() else { return };
        if org.submitting {
            return;
        }
        let token = org.invite_input.read(cx).text().trim().to_string();
        if token.is_empty() {
            org.error = Some("Paste the invitation code from your email".into());
            cx.notify();
            return;
        }
        org.submitting = true;
        org.error = None;
        org.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::ACCEPT_INVITE,
                    serde_json::json!({ "token": token }),
                )
                .await;
            this.update(cx, |shell, cx| {
                if let Some(org) = shell.org.as_mut() {
                    org.submitting = false;
                    if let Err(err) = result {
                        org.error = Some(format!("{err}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn select_org(&mut self, organization_id: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(org) = self.org.as_mut() else { return };
        org.submitting = true;
        org.error = None;
        org.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::SELECT_ORG,
                    serde_json::json!({ "organizationId": organization_id }),
                )
                .await;
            this.update(cx, |shell, cx| {
                if let Some(org) = shell.org.as_mut() {
                    org.submitting = false;
                    if let Err(err) = result {
                        org.error = Some(format!("{err}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    // ---- render pieces ----

    /// Evaluate a width tween at "now" (manual drive — see [`WidthTween`]).
    /// Mid-flight: eased 200ms lerp, and `motion_active` is flagged so render
    /// schedules the next animation frame. Finished, stale, absent, or under
    /// reduced motion: exactly `target`. Honors `COMET_MOTION_SCALE`.
    fn eval_tween(&self, tween: Option<WidthTween>, target: f32) -> f32 {
        let Some(WidthTween { from, to, started }) = tween else {
            return target;
        };
        if self.reduced_motion {
            return target;
        }
        let total = RESIZE.total().mul_f32(motion::speed_scale());
        let raw = started.elapsed().as_secs_f32() / total.as_secs_f32();
        if raw >= 1.0 {
            return target;
        }
        self.motion_active.set(true);
        motion::lerp(from, to, RESIZE.progress(raw))
    }

    /// Animated width container: tweens 200ms ease-out on collapse/expand, and
    /// clips a fixed-width inner so content never reflows mid-transition.
    fn pane_container(
        &self,
        tween: Option<WidthTween>,
        target: f32,
        inner: AnyElement,
    ) -> AnyElement {
        div()
            .h_full()
            .flex_none()
            .overflow_hidden()
            .w(px(self.eval_tween(tween, target)))
            .child(inner)
            .into_any_element()
    }

    /// The animated spacer clearing the macOS traffic lights ahead of a
    /// titlebar control cluster. Fullscreen toggles tween the cluster start
    /// over 200ms ease-out ([`RESIZE`]; reduced motion snaps).
    /// `None` off macOS — no phantom flex child.
    fn titlebar_spacer(&self, container_pad: f32) -> Option<AnyElement> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        let fullscreen = self.fullscreen.unwrap_or(false);
        // The tween runs in cluster-start coordinates; the spacer is that
        // minus the container's own padding.
        let start = self.eval_tween(self.titlebar_tween, titlebar_cluster_start(fullscreen));
        let width = (start - container_pad).max(0.0);
        Some(div().flex_none().h_full().w(px(width)).into_any_element())
    }

    /// The header's content row with the animated left inset — the native port
    /// of comet __root.tsx `transition-[padding-left] duration-200 ease-out` +
    /// `style={{ paddingLeft: headerInset }}`: on sidebar toggles (and macOS
    /// fullscreen flips) the SAME element's padding tweens, so the title
    /// glides to its new x-position. Route changes SNAP: the tween is killed
    /// by every route transition (comet remounts the keyed header variants —
    /// instant swap, zero horizontal motion).
    /// Where unified-titlebar content (tabs / the settings label) starts: past
    /// the traffic lights + control cluster, riding the fullscreen inset tween.
    pub(super) fn title_bar_content_start(&self) -> f32 {
        let fullscreen = self.fullscreen.unwrap_or(false);
        let is_macos = cfg!(target_os = "macos");
        let cluster = self.eval_tween(
            self.titlebar_tween,
            cluster_buttons_start(is_macos, fullscreen),
        );
        // The same 8px clearance [`cluster_clearance`] gives a full-bleed
        // header. On macOS at rest that is 88 + 76 + 8 = 172 — where the canvas
        // starts the tab strip (`docs/design/window.md` claim B3).
        cluster + CLUSTER_BUTTONS_WIDTH + Theme::SPACE_SM
    }

    /// The unified window titlebar: chat → the session tab strip; settings →
    /// the section label. Full-width on the glass shell; the traffic lights
    /// and control cluster overlay its left end.
    fn render_title_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        match self.route {
            // The review route draws the strip too, with the review's own tab
            // leading it (gh#311): the hazard this route used to avoid —
            // switching sessions out from under a review, leaving the card
            // describing one attempt and the column beside it holding another —
            // is gone now that selecting another tab LEAVES the review rather
            // than re-pointing the column under it.
            Route::Chat | Route::Review { .. } => self.render_session_tab_strip(cx),
            Route::Settings(_) => {
                let theme = Theme::of(cx).clone();
                // Settings says so where the tab strip would be
                // (`docs/design/settings.md` A3): one word, 13px/500 `--muted`,
                // at the same x=172 the tabs start at.
                let label = div()
                    .text_size(px(Theme::TEXT_BODY))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted)
                    .child(SharedString::from("Settings"));
                let inner = div()
                    .size_full()
                    .flex()
                    .items_center()
                    .pt(px(Theme::TITLEBAR_TOP_PAD))
                    .pl(px(self.title_bar_content_start()))
                    .pr(px(Theme::SPACE_LG))
                    .child(label);
                let bar = div().h(px(Theme::TITLEBAR_HEIGHT)).flex_none().child(inner);
                self.titlebar_drag_region("settings-header-titlebar", bar, cx)
                    .into_any_element()
            }
        }
    }

    /// Make a titlebar strip drag the window — zed's platform-titlebar
    /// pattern (comet's `.drag` region): mark it a [`WindowControlArea::Drag`]
    /// (macOS app-owned titlebar), hand the drag to the compositor once the
    /// pointer moves with the button down, and double-click zooms.
    fn titlebar_drag_region(
        &self,
        id: &'static str,
        el: gpui::Div,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        el.id(id)
            .window_control_area(WindowControlArea::Drag)
            .on_mouse_down_out(cx.listener(|this, _, _, _| this.titlebar_should_move = false))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.titlebar_should_move = false),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.titlebar_should_move = true),
            )
            // Hand the drag to the compositor only while the button is
            // actually held (`pressed_button` guard): on macOS
            // `start_window_move` runs AppKit's NATIVE drag session
            // (`performWindowDragWithEvent:`), and AppKit resolves a quick
            // second click inside that session as a titlebar double-click —
            // system zoom — natively, beyond gpui's reach. Without the guard a
            // stale `titlebar_should_move` (armed by a down whose bubble was
            // later stopped) would start that session from a mere hover move
            // between the two clicks of a double-click.
            .on_mouse_move(
                cx.listener(|this, event: &gpui::MouseMoveEvent, window, _| {
                    if this.titlebar_should_move && event.pressed_button == Some(MouseButton::Left)
                    {
                        this.titlebar_should_move = false;
                        window.start_window_move();
                    }
                }),
            )
            .on_click(|event, window, _| {
                if event.click_count() == 2 {
                    if cfg!(target_os = "macos") {
                        // Native titlebar double-click action (zoom/minimize
                        // per system preference).
                        window.titlebar_double_click();
                    } else {
                        window.zoom_window();
                    }
                }
            })
    }

    /// The ONE top-left window-control cluster (sidebar toggle + back/forward —
    /// comet window-controls.tsx): rendered once, in a paint-only overlay layer
    /// pinned at the window's top-left, ABOVE the sidebar and headers. The
    /// sidebar width animates *beneath* it, so the buttons keep their element
    /// identity and never move or remount on collapse/expand; only the
    /// fullscreen traffic-light inset tweens (the animated spacer). The
    /// container has no id/listeners — everything between the buttons falls
    /// through to the titlebar drag strips below.
    fn render_titlebar_cluster(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let can_back = self.nav.can_back();
        let can_forward = self.nav.can_forward();
        div()
            .absolute()
            .top_0()
            .left_0()
            .h(px(Theme::TITLEBAR_HEIGHT))
            .flex()
            .flex_row()
            .items_center()
            .pt(px(Theme::TITLEBAR_TOP_PAD))
            .gap(px(2.0))
            .px(px(10.0))
            .children(self.titlebar_spacer(12.0))
            .child(window_control_button(
                "toggle-sidebar",
                icons::SIDEBAR_MINIMALISTIC_LEFT,
                &theme,
                cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)),
            ))
            .child(nav_history_button(
                "nav-back",
                icons::ARROW_LEFT,
                can_back,
                &theme,
                cx.listener(|this, _, _, cx| this.navigate_back(cx)),
            ))
            .child(nav_history_button(
                "nav-forward",
                icons::ARROW_RIGHT,
                can_forward,
                &theme,
                cx.listener(|this, _, _, cx| this.navigate_forward(cx)),
            ))
            .into_any_element()
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let inner: AnyElement = match self.route {
            Route::Settings(section) => self.render_settings_nav(section, &theme, cx),
            // The review route keeps the chat sidebar: the session beside the
            // review is a real selection, and the list is where its space, its
            // siblings and the Active group still live.
            Route::Chat | Route::Review { .. } => self.render_chat_sidebar(&theme, cx),
        };
        let target = self.sidebar_target();
        // Transparent — the sidebar sits directly on the frost shell; the main
        // card's own border provides the separation.
        self.pane_container(
            self.sidebar_tween,
            target,
            div().h_full().child(inner).into_any_element(),
        )
    }

    /// Settings-mode sidebar: "Settings" heading, icon section rows styled
    /// like session rows, then a rule and a `Back` row pinned to the bottom.
    ///
    /// No account footer (gh#258). The chat sidebar closes with one and this
    /// one deliberately does not: the supplied Settings design ends the column
    /// at `Back`, and the reason holds up — the account block's job is to say
    /// who you are while you are working somewhere that might not be your own
    /// machine, and Settings is where the Accounts page answers that question
    /// properly, at length, three feet to the right.
    fn render_settings_nav(
        &mut self,
        section: SettingsSection,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        /// The nav's own side padding, inside the column's 8px gutter
        /// (`docs/design/settings.md` B1/B2) — the same 9 the Back row's
        /// [`account::footer_row`] uses, so heading, rows and Back share one
        /// left edge at x=17.
        const NAV_PAD_X: f32 = 9.0;
        let section_icon = |item: SettingsSection| match item {
            SettingsSection::Devices => icons::MONITOR,
            SettingsSection::Agents => icons::KEY_MINIMALISTIC,
            // The embedded Solar set has no people glyph; the workspace roster
            // is a list of who is in it.
            SettingsSection::Members => icons::CHECKLIST,
            SettingsSection::Appearance => icons::TUNING,
            SettingsSection::Shortcuts => icons::KEYBOARD,
            SettingsSection::Routing => icons::CHECKLIST,
            // The rules run the board unattended; the wand is the closest
            // glyph the embedded set has to "it happens by itself".
            SettingsSection::Automations => icons::MAGIC_STICK,
            SettingsSection::Stats => icons::CHART,
            SettingsSection::Archived => icons::ARCHIVE_MINIMALISTIC,
        };
        // Match the user's dragged sidebar width — the pane container clips to
        // it, so a hardcoded default here left hover washes stopping short of
        // the sidebar's right edge (user-reported). Device identity lives on
        // the Accounts page now — the one surface where the device matters.
        div()
            .w(px(self.settings.sidebar_width))
            .h_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_1()
                    .px(px(Theme::SPACE_SM))
                    .flex()
                    .flex_col()
                    .child(
                        // `docs/design/settings.md` B1: 10/9/6 at 11px/600.
                        div()
                            .px(px(NAV_PAD_X))
                            .pt(px(10.0))
                            .pb(px(6.0))
                            .text_size(px(Theme::TEXT_CAPTION))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text_subtle)
                            .child(SharedString::from("Settings")),
                    )
                    .child(div().flex().flex_col().gap(px(3.0)).children(
                        SettingsSection::ALL.into_iter().map(|item| {
                            let selected = item == section;
                            div()
                                .id(SharedString::from(format!("settings-nav-{}", item.label())))
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(9.0))
                                .rounded(px(Theme::RADIUS_ROW))
                                .px(px(NAV_PAD_X))
                                .py(px(6.0))
                                .text_size(px(Theme::TEXT_BODY))
                                .when(selected, |el| el.font_weight(gpui::FontWeight::MEDIUM))
                                .cursor_pointer()
                                .list_row(
                                    &theme,
                                    Bed::Shell,
                                    selected,
                                    format!("settings-nav-{}", item.label()),
                                )
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.open_settings(item, cx)),
                                )
                                .child(
                                    // The glyph moves with its label (B5): the
                                    // selected row's icon is `--text`, the rest
                                    // sit one tone under their own copy. An icon
                                    // held at `--muted` on the selected row made
                                    // the brightened label look mismatched with
                                    // the mark that introduces it.
                                    icon(section_icon(item)).size(px(16.0)).text_color(
                                        if selected {
                                            theme.text
                                        } else {
                                            theme.text_subtle
                                        },
                                    ),
                                )
                                .child(SharedString::from(item.label()))
                        }),
                    )),
            )
            // Back pinned to the bottom, under the same hairline the chat
            // sidebar draws over its footer: the rule is what makes the row
            // read as the end of the column rather than a ninth nav item.
            .child(
                // The nav already owns an 8px gutter; the divider has its own
                // 8px inset inside that, matching the reference's x=16…240.
                div()
                    .px(px(Theme::SPACE_SM))
                    .child(account::footer_divider(theme)),
            )
            .child(
                div().px(px(Theme::SPACE_SM)).my(px(Theme::SPACE_SM)).child(
                    account::footer_row(theme, "settings-back")
                        .h(px(30.0))
                        .py(px(0.0))
                        .gap(px(7.0))
                        .text_size(px(Theme::TEXT_BODY))
                        .text_color(theme.text_muted)
                        .hover(|s| s.text_color(theme.text))
                        .on_click(cx.listener(|this, _, _, cx| this.close_settings(cx)))
                        .child(
                            // AltArrowLeft chevron (comet settings-sidebar.tsx),
                            // not the straight history arrow — and `--subtle`
                            // under `--muted` copy, like every other nav row
                            // (`docs/design/settings.md` B8).
                            icon(icons::ALT_ARROW_LEFT)
                                .size(px(16.0))
                                .text_color(theme.text_subtle),
                        )
                        .child(SharedString::from("Back")),
                ),
            )
            .into_any_element()
    }

    /// Which sidebar-list edges have hidden overflow (offset from the LAST
    /// frame — the invisible one-frame lag every fade here rides).
    pub(super) fn sidebar_fade_zones(&self) -> (bool, bool) {
        let scrolled = -f32::from(self.sidebar_scroll.offset().y);
        let max_scroll = f32::from(self.sidebar_scroll.max_offset().y);
        (scrolled > 1.0, scrolled < max_scroll - 1.0)
    }

    /// Chat-mode sidebar: the Needs-you inbox, the orchestrator's pinned
    /// fixture, then **Spaces** — each space disclosing its own sessions
    /// inline, with the Unfiled tail for runs no space names — then the
    /// notice strip and the account footer (gh#230, gh#547).
    ///
    /// gh#258 put the hierarchy back. Active used to be a group of its own
    /// between the inbox and the tree, holding the full row of every live run
    /// and leaving the tree to enumerate what was left; the orchestrator had a
    /// pinned slot above that. Three groups deep, the reader had to know which
    /// surface a chat was on this frame before they could look for it, and the
    /// thing the window is actually organised by — the spaces — started a
    /// third of the way down the column. So the tree came to own every row: a
    /// live run's row is drawn inside its own space's disclosure, above that
    /// space's idle sessions. gh#547 finished the job: the orchestrator is the
    /// board's voice and not a chat in a folder, so it left the selected
    /// space's disclosure for its own fixture between the inbox and Spaces,
    /// and what was left of Active — live runs whose chat names no space at
    /// all, the one case a disclosure cannot draw — moved INSIDE Spaces as its
    /// Unfiled tail instead of masquerading as a section beside it. Two
    /// sections and a pin.
    fn render_chat_sidebar(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        // Overflow edge fades for the lists scroll region — the tab strip's
        // idiom, vertical (offset from the LAST frame; the lag is invisible).
        let (lists_fade_top, lists_fade_bottom) = self.sidebar_fade_zones();
        // Opaque platforms melt overflow into the surface tone with painted
        // gradient overlays. Over GLASS no overlay can work — the backdrop is
        // see-through blur, so tone stacks into a smudge and black reads as a
        // shadow (user reports). Instead the ROWS fade themselves: prepaint-
        // measured bounds drive per-row opacity toward the viewport edges
        // ([`Shell::sidebar_row_alpha`]), dissolving the edge to pure glass.
        let glass = Theme::GLASS_ALPHA < 1.0;
        let sidebar_fade = theme.surface;

        let account_footer = self.render_account_footer(theme, cx);

        // The sidebar is two sections and a pin (gh#547). First, the inbox
        // (gh#122): does anything want me — in words, and it cannot miss; it
        // is a projection over what follows, not a fourth place things live.
        // Then the orchestrator's pinned fixture — the board's voice, which
        // belongs to no space and so sits outside the tree. Then Spaces,
        // which owns where everything else lives: each space's disclosure,
        // and the Unfiled tail for live runs no space claims.
        let needs_section = self.render_needs_section(theme, cx);
        let orchestrator_fixture = self.render_orchestrator_fixture(theme, cx);
        // Everything alive (gh#103/gh#117), derived ONCE here, because the
        // tree below is defined by it: each space's disclosure draws the live rows
        // that belong to it and then its idle ones. One derivation, so the two
        // halves of a space's list can never disagree about which chats are
        // running this frame.
        let active_now = Utc::now();
        let active = self.board.read(cx).active(cx, active_now);
        let placements: Vec<(String, Option<String>)> = {
            let state = self.state.read(cx);
            comet_proto::view::spaces::active_placements(&active, &state.chats, &state.spaces)
                .into_iter()
                .map(|(chat, space)| (chat.to_string(), space.map(str::to_string)))
                .collect()
        };
        let spaces_section = self.render_spaces_section(&active, &placements, theme, cx);

        div()
            .w(px(self.settings.sidebar_width))
            .h_full()
            .flex()
            .flex_col()
            // (No titlebar strip: the unified window titlebar spans the whole
            // window above this column.)
            // Spaces + the global Sessions list share one scroll region. On
            // glass the whole region paints inside an EdgeFade scope — a true
            // per-glyph gradient at active overflow edges.
            .child(crate::edge_fade::edge_faded(
                SIDEBAR_GLASS_FADE_BAND,
                glass && lists_fade_top,
                glass && lists_fade_bottom,
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("sidebar-lists")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.sidebar_scroll)
                            .px(px(Theme::SPACE_SM))
                            .flex()
                            .flex_col()
                            .child(needs_section)
                            .children(orchestrator_fixture)
                            // The section divider the canvas draws above the
                            // Spaces header (`docs/design/window.md` C2.1) —
                            // the one hairline that says "everything above
                            // this line wants you; everything below it just
                            // lives somewhere".
                            .child(Self::render_sidebar_rule(theme))
                            .child(spaces_section)
                            .child(div().pb(px(Theme::SPACE_SM))),
                    )
                    .when(lists_fade_top && !glass, |el| {
                        el.child(div().absolute().top_0().left_0().right_0().h(px(24.0)).bg(
                            gpui::linear_gradient(
                                180.0,
                                gpui::linear_color_stop(sidebar_fade, 0.0),
                                gpui::linear_color_stop(sidebar_fade.opacity(0.0), 1.0),
                            ),
                        ))
                    })
                    .when(lists_fade_bottom && !glass, |el| {
                        el.child(
                            div()
                                .absolute()
                                .bottom_0()
                                .left_0()
                                .right_0()
                                .h(px(24.0))
                                .bg(gpui::linear_gradient(
                                    0.0,
                                    gpui::linear_color_stop(sidebar_fade, 0.0),
                                    gpui::linear_color_stop(sidebar_fade.opacity(0.0), 1.0),
                                )),
                        )
                    }),
            ))
            // Update strip (above the account footer; below the lists).
            .when_some(self.render_update_strip(theme, cx), |el, strip| {
                el.child(strip)
            })
            // Inline mutation-failure notice.
            .when_some(self.sidebar_notice.clone(), |el, notice| {
                el.child(
                    div()
                        .id("sidebar-notice")
                        .mx(px(Theme::SPACE_SM))
                        .mb(px(Theme::SPACE_SM))
                        .px(px(Theme::SPACE_SM))
                        .py(px(4.0))
                        .rounded(px(Theme::RADIUS_CHIP))
                        .border_1()
                        .border_color(theme.danger)
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(theme.danger)
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.sidebar_notice = None;
                            cx.notify();
                        }))
                        .child(notice),
                )
            })
            .child(account_footer)
            .into_any_element()
    }

    /// Update strip: shown above the user menu whenever the engine's
    /// UpdateStatus stream reports a newer release. On a macOS bundle install
    /// it drives the whole flow — click to download, then click to restart into
    /// the staged bundle. Elsewhere (managed/source installs) it is advisory
    /// (`comet update`); click dismisses it for that version.
    fn render_update_strip(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let status = self.state.read(cx).update.clone()?;
        if !status.update_available {
            return None;
        }
        let latest = status.latest_version.clone()?;
        if self.update_dismissed.as_deref() == Some(latest.as_str()) {
            return None;
        }
        let mac_app = matches!(self.install, comet_update::InstallKind::MacApp { .. });

        let (label, clickable): (SharedString, bool) = if mac_app {
            match &self.update_flow {
                UpdateFlow::Idle => (format!("Update available — v{latest}").into(), true),
                UpdateFlow::Downloading => (format!("Downloading v{latest}…").into(), false),
                UpdateFlow::Ready(_) => ("Update ready — restart to apply".into(), true),
                UpdateFlow::Failed(message) => (format!("Update failed: {message}").into(), true),
            }
        } else {
            (
                format!("Update available — v{latest} · run `comet update`").into(),
                true,
            )
        };
        let failed = matches!(self.update_flow, UpdateFlow::Failed(_));
        let tone = if failed { theme.danger } else { theme.accent };
        // The chip fill is the sidebar's WHITE wash language, not an accent
        // tint: an indigo fill over the glass composited into a dark slab that
        // blocked the blur (user report) — the accent lives in the icon/text.
        let (chip_bg, chip_bg_hover) = if failed {
            (theme.danger.opacity(0.14), theme.danger.opacity(0.22))
        } else {
            (theme.wash(0.11), theme.wash(0.16))
        };

        let mut strip = div()
            .id("update-strip")
            .mx(px(Theme::SPACE_SM))
            // No bottom margin: the user-menu block below carries its own
            // SPACE_SM padding — doubling it read as a hole (user report).
            .px(px(Theme::SPACE_SM))
            .py(px(6.0))
            .rounded(px(Theme::RADIUS_CHIP))
            .bg(chip_bg)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .text_size(px(Theme::TEXT_CAPTION))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(tone)
            .child(
                icon(if failed {
                    icons::DANGER_TRIANGLE
                } else {
                    icons::RESTART
                })
                .size(px(14.0))
                .text_color(tone),
            )
            .child(div().flex_1().min_w_0().child(label));
        if clickable {
            strip = strip
                .cursor_pointer()
                .hover(move |s| s.bg(chip_bg_hover))
                .on_click(cx.listener(move |this, _, _, cx| this.on_update_strip_click(cx)));
        }
        Some(strip.into_any_element())
    }

    /// Idle → download; Ready → swap + relaunch; Failed → retry; advisory
    /// installs → dismiss for this version.
    fn on_update_strip_click(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.install, comet_update::InstallKind::MacApp { .. }) {
            self.update_dismissed = self
                .state
                .read(cx)
                .update
                .as_ref()
                .and_then(|s| s.latest_version.clone());
            cx.notify();
            return;
        }
        match std::mem::replace(&mut self.update_flow, UpdateFlow::Idle) {
            UpdateFlow::Idle | UpdateFlow::Failed(_) => self.begin_update_download(cx),
            UpdateFlow::Downloading => self.update_flow = UpdateFlow::Downloading,
            UpdateFlow::Ready(staged) => self.apply_staged_update(staged, cx),
        }
    }

    /// Fetch the manifest and stage the new `Comet.app` under the data dir
    /// (tokio — reqwest); the strip flips to "restart to apply" when done.
    fn begin_update_download(&mut self, cx: &mut Context<Self>) {
        let edge_url = self.boot.edge_url.clone();
        let data_dir = self.data_dir.clone();
        self.update_flow = UpdateFlow::Downloading;
        let download = Tokio::spawn(cx, async move {
            let manifest = comet_update::fetch_latest(&edge_url).await?;
            comet_update::stage_mac_app(&edge_url, &manifest, &data_dir).await
        });
        self.update_task = Some(cx.spawn(async move |this, cx| {
            let outcome = match download.await {
                Ok(Ok(staged)) => Ok(staged),
                Ok(Err(err)) => Err(format!("{err:#}")),
                Err(join_err) => Err(join_err.to_string()),
            };
            this.update(cx, |shell, cx| {
                shell.update_flow = match outcome {
                    Ok(staged) => UpdateFlow::Ready(staged),
                    Err(message) => {
                        tracing::warn!(%message, "update download failed");
                        UpdateFlow::Failed(message.into())
                    }
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Swap the staged bundle over the installed one, arm the detached
    /// relauncher, and quit — the relauncher `open`s the new bundle once this
    /// process (and its engine lock / IPC port) is gone.
    fn apply_staged_update(&mut self, staged: PathBuf, cx: &mut Context<Self>) {
        let comet_update::InstallKind::MacApp { bundle } = self.install.clone() else {
            return;
        };
        match comet_update::apply_mac_app(&staged, &bundle) {
            Ok(()) => {
                comet_update::relaunch_app_after_exit(&bundle);
                cx.quit();
            }
            Err(err) => {
                tracing::error!(error = %err, "update apply failed");
                self.update_flow = UpdateFlow::Failed(format!("{err:#}").into());
                cx.notify();
            }
        }
    }

    /// The account footer (gh#230): a hairline, then the block that closes
    /// every sidebar — avatar, display name, plan tier — and the account menu
    /// it triggers.
    ///
    /// Every word comes from [`comet_proto::view::account::account_footer`],
    /// derived from the engine's last `AuthStatus`. That is the whole point of
    /// the block: the board sweeps hosts and the priced pages read rates off
    /// whichever account is signed in, so a footer that guessed would be worse
    /// than none. Signed out it degrades to a row that says so and offers the
    /// way in, rather than to an empty strip.
    ///
    /// That signed-out row is narrow but real: [`comet_proto::view::gate_phase`]
    /// puts a reported `SignedOut` behind the sign-in gate and a workspace-less
    /// session behind the org gate, so the one state that reaches here without
    /// a session is the window between the engine connecting and its first
    /// `AuthStatus` frame. The row still has to be right for it — that gap is
    /// exactly when "am I signed in?" is the question being asked.
    fn render_account_footer(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let identity = view_account::account_footer(self.state.read(cx).auth.as_ref());
        let open = self.user_menu_open;
        let in_settings = matches!(self.route, Route::Settings(_));

        let mut block = account::account_block(theme, &identity, open)
            .my(px(Theme::SPACE_SM))
            .on_click(cx.listener(|this, _, _, cx| {
                // A click that just dismissed the menu (outside-click on the
                // trigger) must not instantly reopen it.
                let just_dismissed = this
                    .user_menu_dismissed_at
                    .is_some_and(|at| at.elapsed() < Duration::from_millis(400));
                this.user_menu_open = !this.user_menu_open && !just_dismissed;
                this.user_menu_dismissed_at = None;
                cx.notify();
            }));

        if open {
            // user-menu.tsx content: `w-[--radix-dropdown-menu-trigger-width]`
            // (exactly as wide as the trigger row — sidebar minus its p-2
            // gutters), `flex-col gap-0.5`, then: one small muted email line
            // (`px-2 pb-1 pt-1.5 text-[11px] text-muted-foreground/70`),
            // "Settings", separator, and the session verb. Both rows are plain
            // `menuItem`s with muted 16px icons — sign-out carries NO
            // destructive tone in the original.
            let signed_out = !identity.state.has_session();
            let menu = popover::popover_card(theme)
                .w(px(self.settings.sidebar_width - 2.0 * Theme::SPACE_SM))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.user_menu_open = false;
                    this.user_menu_dismissed_at = Some(std::time::Instant::now());
                    cx.notify();
                }))
                .flex()
                .flex_col()
                .gap(px(2.0))
                // The address, when there is one — signed out there is nothing
                // to caption the name with, and a blank line is not a caption.
                .when_some(identity.email.clone(), |el, email| {
                    el.child(
                        div()
                            .px(px(8.0))
                            .pt(px(6.0))
                            .pb(px(4.0))
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(theme.text_subtle)
                            .truncate()
                            .child(SharedString::from(email)),
                    )
                })
                // Settings is a door, and on the settings route it leads where
                // you already are.
                .when(!in_settings, |el| {
                    el.child(
                        popover::menu_row(theme, false, "user-menu-settings")
                            .id("user-menu-settings")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_settings(SettingsSection::Devices, cx)
                            }))
                            .child(
                                icon(icons::SETTINGS_MINIMALISTIC)
                                    .size(px(16.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from("Settings")),
                    )
                })
                .when(!in_settings, |el| el.child(popover::menu_separator(theme)))
                // One verb, and it is the one the session state allows: a
                // signed-out window offering "Sign out" was the same lie the
                // hardcoded plan told.
                .child(if signed_out {
                    popover::menu_row(theme, false, "user-menu-signin")
                        .id("user-menu-signin")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.user_menu_open = false;
                            this.start_sign_in(cx);
                            cx.notify();
                        }))
                        .child(
                            icon(icons::LOGIN_2)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Sign in"))
                } else {
                    popover::menu_row(theme, false, "user-menu-signout")
                        .id("user-menu-signout")
                        .on_click(cx.listener(|this, _, _, cx| this.sign_out(cx)))
                        .child(
                            icon(icons::LOGOUT_2)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Sign out"))
                })
                .into_any_element();
            block = block.child(popover::anchored_menu_above("user-menu-popover", menu));
        }

        div()
            .flex_none()
            .px(px(Theme::SPACE_SM))
            .child(account::footer_divider(theme))
            .child(block)
            .into_any_element()
    }

    /// Floating layers owned by the shell: the session context menu and the
    /// rename / delete-confirm dialogs.
    fn render_overlays(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = Theme::of(cx).clone();
        let mut overlays: Vec<AnyElement> = Vec::new();

        if let Some((chat_id, position)) = self.chat_menu.clone() {
            let rename_id = chat_id.clone();
            let archive_id = chat_id.clone();
            let delete_id = chat_id.clone();
            // Unpinning is the same item on the row that is pinned: whoever
            // wants the notices to stop reaches for the session they pinned,
            // not for a settings page.
            let pinned = self.state.read(cx).is_orchestrator(&chat_id);
            let pin_target = (!pinned).then(|| chat_id.clone());
            let menu = popover::popover_card(&theme)
                .w(px(170.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.chat_menu = None;
                    cx.notify();
                }))
                .flex()
                .flex_col()
                .child(
                    popover::menu_row(&theme, false, format!("chat-menu-rename-{chat_id}"))
                        .id("chat-menu-rename")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_rename_chat(rename_id.clone(), cx)
                        }))
                        .child(icon(icons::PEN).size(px(16.0)).text_color(theme.text_muted))
                        .child(SharedString::from("Rename…")),
                )
                .child(
                    popover::menu_row(&theme, false, format!("chat-menu-archive-{chat_id}"))
                        .id("chat-menu-archive")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.archive_chat(archive_id.clone(), cx)
                        }))
                        .child(
                            icon(icons::ARCHIVE_MINIMALISTIC)
                                .size(px(16.0))
                                .text_color(theme.text_muted),
                        )
                        .child(SharedString::from("Archive")),
                )
                .child(popover::menu_separator(&theme))
                .child(
                    popover::menu_row(&theme, false, format!("chat-menu-pin-{chat_id}"))
                        .id("chat-menu-pin")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.set_orchestrator(pin_target.clone(), cx)
                        }))
                        .child(icon(icons::PIN).size(px(16.0)).text_color(if pinned {
                            theme.accent
                        } else {
                            theme.text_muted
                        }))
                        .child(SharedString::from(if pinned {
                            "Unpin as orchestrator"
                        } else {
                            "Pin as orchestrator"
                        })),
                )
                .child(popover::menu_separator(&theme))
                .child(
                    popover::menu_row(&theme, false, format!("chat-menu-delete-{chat_id}"))
                        .id("chat-menu-delete")
                        .text_color(theme.danger)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.chat_menu = None;
                            this.delete_confirm = Some(delete_id.clone());
                            cx.notify();
                        }))
                        .child(
                            icon(icons::TRASH_BIN_MINIMALISTIC)
                                .size(px(16.0))
                                .text_color(theme.danger),
                        )
                        .child(SharedString::from("Delete…")),
                )
                .into_any_element();
            overlays.push(popover::menu_at("chat-context-menu", position, menu));
        }

        if let Some(dialog) = &mut self.rename_dialog {
            if std::mem::take(&mut dialog.focus_pending) {
                window.focus(&dialog.input.focus_handle(cx), cx);
            }
            let input = dialog.input.clone();
            let card = popover::dialog_card(&theme)
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" {
                        this.rename_dialog = None;
                        cx.notify();
                    }
                }))
                .child(popover::dialog_title(&theme, "Rename session"))
                .child(
                    div()
                        .mt(px(12.0))
                        .child(popover::dialog_field(&theme, input.into_any_element())),
                )
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "rename-chat-cancel")
                                .id("rename-chat-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.rename_dialog = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Rename")
                                .id("rename-chat-save")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.submit_rename_chat(cx)),
                                ),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("rename-chat-dialog", viewport, card));
        }

        if let Some(overlay) = self.render_fork_dialog(viewport, cx) {
            overlays.push(overlay);
        }

        overlays.extend(self.render_space_overlays(viewport, window, cx));
        if let Some(overlay) = self.render_add_space_overlay(viewport, window, cx) {
            overlays.push(overlay);
        }

        if let Some(selected) = self.new_session_chooser {
            let rows: Vec<AnyElement> = self
                .state
                .read(cx)
                .spaces
                .iter()
                .enumerate()
                .map(|(index, space)| {
                    let id = space.id.clone();
                    popover::menu_row(&theme, index == selected, format!("new-session-space-{id}"))
                        .id(format!("new-session-space-row-{id}"))
                        .on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.new_session(Some(id.clone()), cx)
                            }),
                        )
                        .child(SharedString::from(space.display_name().to_string()))
                        .into_any_element()
                })
                .collect();
            let card = popover::dialog_card(&theme)
                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                    if event.keystroke.key == "escape" {
                        cx.stop_propagation();
                        this.new_session_chooser = None;
                        cx.notify();
                    }
                }))
                .child(popover::dialog_title(&theme, "New Session"))
                .child(div().mt(px(6.0)).child(popover::dialog_body(
                    &theme,
                    if rows.is_empty() {
                        "No workspaces yet. Press Enter to add one."
                    } else {
                        "Choose a workspace with ↑/↓ and Enter."
                    },
                )))
                .child(
                    div()
                        .mt(px(12.0))
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .children(rows),
                );
            overlays.push(popover::modal(
                "new-session-chooser",
                viewport,
                card.into_any_element(),
            ));
        }

        if let Some(chat_id) = self.delete_confirm.clone() {
            let title = transcript::single_line(
                &self
                    .state
                    .read(cx)
                    .chats
                    .iter()
                    .find(|c| c.id == chat_id)
                    .and_then(|c| c.title.clone())
                    .unwrap_or_else(|| "New session".into()),
            );
            let card = popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Delete session?"))
                .child(div().mt(px(6.0)).child(popover::dialog_body(
                    &theme,
                    format!("\u{201C}{title}\u{201D} will be permanently deleted. This can\u{2019}t be undone."),
                )))
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "delete-chat-cancel")
                                .id("delete-chat-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.delete_confirm = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_danger(&theme, "Delete")
                                .id("delete-chat-confirm")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.delete_chat(chat_id.clone(), cx)
                                })),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("delete-chat-dialog", viewport, card));
        }

        overlays
    }

    fn resize_handle<T>(
        &self,
        id: &'static str,
        marker: fn() -> T,
        reset: fn(&mut Shell, &mut Context<Shell>),
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div>
    where
        T: 'static,
    {
        let hover = Theme::of(cx).border_strong;
        div()
            .id(id)
            .w(px(5.0))
            .h_full()
            .flex_none()
            .cursor_col_resize()
            .hover(move |s| s.bg(hover))
            .on_drag(marker(), |_, _point: Point<gpui::Pixels>, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DragGhost)
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                    if event.click_count == 2 {
                        reset(this, cx);
                        this.schedule_save(cx);
                        cx.notify();
                    }
                }),
            )
    }

    fn render_main(&mut self, cx: &mut Context<Self>) -> AnyElement {
        // Settings route: just the section outlet — the section label lives in
        // the unified window titlebar now (render_title_bar).
        if let Route::Settings(section) = self.route {
            let outlet = self.settings_outlet(section, cx);
            return div()
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .flex_col()
                .child(div().flex_1().min_h_0().child(outlet))
                .into_any_element();
        }
        if matches!(self.route, Route::Review { .. }) {
            return self.render_review_route(cx);
        }
        self.render_conversation(true, cx)
    }

    /// The review route (gh#180): the same two things as every other route,
    /// swapped.
    ///
    /// Everywhere else the conversation is the content and a diff sits in a
    /// dock beside it. Reviewing inverts that, because reviewing inverts what
    /// you are doing: the changes are what you came to read and the chat is the
    /// reference you consult about them. So the review takes the card, and the
    /// session becomes the narrow column in the dock's own slot on the right
    /// (gh#276) — the reference goes where every other reference in this app
    /// goes, and the two are separate inset cards with a seam between them
    /// rather than one card split by a hairline.
    fn render_review_route(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(panel) = self.review.clone() else {
            // Only reachable if the route outlived its panel; the chat route is
            // the honest fallback rather than an empty card.
            return self.render_conversation(true, cx);
        };
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_hidden()
            .child(panel)
            .into_any_element()
    }

    /// The authoring session, as the review route's right-hand card
    /// (review.md I).
    ///
    /// It keeps its composer, because the most useful thing a reviewer can do
    /// with an unclaimed change is ask the agent that made it, and it drops the
    /// terminal dock, which has no room to be anything but a letterbox at this
    /// width. Above the composer sits the delivery preview — the payload is a
    /// message that will arrive in *this* column, and this is where the canvas
    /// shows it waiting.
    fn render_review_session(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let width = self
            .settings
            .review_session_width
            .clamp(REVIEW_SESSION_MIN, REVIEW_SESSION_MAX);
        let has_chat = matches!(&self.route, Route::Review { chat_id, .. } if chat_id.is_some());
        // A review outlives the chat that produced it — that is why the claims
        // live on the attempt (gh#183) — so the column says where the session
        // went instead of drawing an empty transcript.
        let session: AnyElement = if has_chat {
            self.render_conversation(false, cx)
        } else {
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .px(px(Theme::SPACE_LG))
                .child(
                    div()
                        .text_size(px(Theme::TEXT_BODY))
                        .text_color(theme.text_faint)
                        .child(SharedString::from(
                            "The session that wrote this is gone. The review is not.",
                        )),
                )
                .into_any_element()
        };
        // The seam is on this card's LEFT now, so that is the edge that drags.
        let handle = self
            .resize_handle(
                "review-session-resize",
                || ReviewSessionResize,
                |shell, _| shell.settings.review_session_width = REVIEW_SESSION_DEFAULT,
                cx,
            )
            .absolute()
            .top_0()
            .bottom_0()
            .left(px(0.0));
        let card = div()
            .size_full()
            .flex()
            .flex_col()
            .rounded(px(Theme::RADIUS_CARD))
            .border_1()
            .border_color(theme.border)
            .bg(theme.bg)
            .overflow_hidden()
            .children(self.render_review_session_header(&theme, cx))
            .child(session);
        div()
            .flex_none()
            .w(px(width))
            .h_full()
            .relative()
            .overflow_hidden()
            .pb(px(Theme::SPACE_SM))
            .pr(px(Theme::SPACE_SM))
            .child(card)
            .child(handle)
            .into_any_element()
    }

    /// The verdict payload, drawn at the bottom of the session column
    /// (review.md I4) and composed by the review card itself, so the two
    /// cannot disagree about what is about to be sent.
    ///
    /// `None` off the review route, and on a review with no pull request to
    /// post to — there is no payload, and a dashed empty card promising one
    /// would be a promise about nothing.
    fn render_delivery_preview(&self, theme: &Theme, cx: &App) -> Option<AnyElement> {
        if !matches!(self.route, Route::Review { .. }) {
            return None;
        }
        self.review.as_ref()?.read(cx).delivery_preview(theme, cx)
    }

    /// The session card's header (review.md I1): whose session this is, and
    /// whether anything is happening in it.
    ///
    /// Absent when the chat is gone — there is no session to name, and a header
    /// over the "the session that wrote this is gone" line would be a frame
    /// around an absence.
    fn render_review_session_header(&self, theme: &Theme, cx: &App) -> Option<AnyElement> {
        let state = self.state.read(cx);
        let chat = state.selected_chat_row()?;
        let now = Utc::now();
        let status = comet_proto::view::display_status(chat, state.session_for(&chat.id), now);
        // The same dot the sidebar and the tabs paint: one ramp, so a working
        // session is one colour wherever it is drawn.
        let dot = crate::shell::spaces::status_dot_color(status, theme);
        // The branch is what the review talks about; the title is what the
        // sidebar calls it. This column belongs to the review, so it leads with
        // the branch and falls back to the title only where there is none.
        let name = chat
            .branch
            .clone()
            .filter(|branch| !branch.is_empty())
            .or_else(|| chat.title.clone())
            .unwrap_or_else(|| "Session".to_string());
        // What the session is doing, and since when. One word each, and the
        // one that matters is `waiting` — a session that has asked the reviewer
        // something is the reason to look at this column at all.
        let word = match status {
            comet_proto::ChatIndicator::Working => "working",
            comet_proto::ChatIndicator::AwaitingInput => "waiting",
            comet_proto::ChatIndicator::Errored => "errored",
            comet_proto::ChatIndicator::Completed | comet_proto::ChatIndicator::Idle => "idle",
        };
        let elapsed = match chat.last_message_at {
            Some(at) => format!("{word} · {}", format_time_ago(at, now)),
            None => word.to_string(),
        };
        Some(
            div()
                .flex_none()
                .h(px(40.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(Theme::SPACE_SM))
                .px(px(14.0))
                .border_b_1()
                .border_color(theme.border)
                // round-ok: the status dot every chat row wears, at its size.
                .child(div().flex_none().size(px(5.0)).rounded_full().bg(dot))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(Theme::TEXT_BODY))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.text)
                        .child(SharedString::from(name)),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(px(Theme::TEXT_DENSE))
                        .text_color(theme.text_subtle)
                        .child(SharedString::from(elapsed)),
                )
                .into_any_element(),
        )
    }

    /// The conversation column: transcript (or one of the two canvases), the
    /// reserved status strip, the composer, and — unless `with_terminal` is
    /// false — the terminal dock under all three.
    ///
    /// `with_terminal` exists for the review route, whose session column is
    /// ~420px wide: a terminal there is a letterbox, and the dock's own toggle
    /// is chat-route chrome anyway.
    fn render_conversation(&mut self, with_terminal: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme_owned = Theme::of(cx).clone();
        let theme = &theme_owned;
        let theme_bg = theme.bg;
        let (border, text, faint) = (theme.border, theme.text, theme.text_faint);
        let _ = (text, border);
        let has_selection = self.state.read(cx).selected_chat.is_some();
        let has_spaces = !self.state.read(cx).spaces.is_empty();
        let space_name: SharedString = self
            .state
            .read(cx)
            .selected_space_row()
            .map(|s| s.display_name().to_string())
            .unwrap_or_default()
            .into();

        // Content outlet: selected chat → transcript; nothing selected → the
        // "Send a message to start" canvas with a watermark; no spaces at all
        // → the onboarding card. The composer sits below the first two
        // (new-chat mode mints the chat id on first send).
        let outlet: AnyElement = if has_selection {
            // A fork says so above its own transcript (gh#425): in a shared
            // checkout the diff below is not only this session's work, and a
            // reader who cannot see that will read two agents as one.
            match self.render_lineage_strip(cx) {
                Some(strip) => div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .min_h_0()
                    .child(strip)
                    .child(div().flex_1().min_h_0().child(self.transcript.clone()))
                    .into_any_element(),
                None => self.transcript.clone().into_any_element(),
            }
        } else if !has_spaces {
            // Onboarding (first boot / after the destructive wipe): no folders
            // to work in yet — one clear affordance.
            let _ = faint;
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .child(motion::fade_in(
                    "no-spaces-canvas",
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .child(
                            icon(icons::COMET_LOGO)
                                .w(px(41.9))
                                .h(px(48.0))
                                .text_color(theme.white_alpha(0.09)),
                        )
                        .child(
                            div()
                                .mt(px(24.0))
                                .text_size(px(Theme::TEXT_TITLE))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(SharedString::from("Pick a repo to get started")),
                        )
                        .child(
                            div()
                                .mt(px(6.0))
                                .text_size(px(Theme::TEXT_BODY))
                                .text_color(theme.text_muted)
                                .child(SharedString::from(
                                    // gh#118: the picker's front door is repos,
                                    // so the empty state names the same thing it
                                    // will open on. The folders are still behind
                                    // it; they are not what to lead with.
                                    "Your GitHub repos, running on the box.",
                                )),
                        )
                        .child(
                            popover::btn_primary(&theme_owned, "Add a repo")
                                .id("onboarding-add-space")
                                .mt(px(20.0))
                                .on_click(cx.listener(|this, _, _, cx| this.open_add_space(cx))),
                        ),
                ))
                .into_any_element()
        } else {
            // New-chat canvas (comet index.tsx): the dim comet mark watermark
            // (a 9% wash, ornament rather than text) over the helper line —
            // now naming the space the session will start in.
            let helper: SharedString = if space_name.is_empty() {
                "Send a message to start a new session.".into()
            } else {
                format!("Send a message to start a session in {space_name}.").into()
            };
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .child(motion::fade_in(
                    "new-chat-canvas",
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .child(
                            icon(icons::COMET_LOGO)
                                .w(px(41.9))
                                .h(px(48.0))
                                .text_color(theme.white_alpha(0.09)),
                        )
                        .child(
                            div()
                                .mt(px(24.0))
                                .text_size(px(Theme::TEXT_BODY))
                                .text_color(theme.text_muted)
                                .child(helper),
                        ),
                ))
                .into_any_element()
        };

        let status = self.render_status_strip(cx);
        // File dropzone over the ENTIRE conversation column (transcript +
        // composer, not just the pill): dragging OS files anywhere across the
        // chat area shows the "Drop images to attach" veil; a drop stages the
        // files in the composer. `has_active_drag` gates the veil so a drag
        // that left the window (FileDrop Exited) can't strand it.
        let file_drag_active = self.file_drag_active && cx.has_active_drag();
        div()
            .id("chat-dropzone")
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .on_drag_move::<gpui::ExternalPaths>(cx.listener(
                |this, e: &gpui::DragMoveEvent<gpui::ExternalPaths>, _, cx| {
                    let inside = e.bounds.contains(&e.event.position);
                    if this.file_drag_active != inside {
                        this.file_drag_active = inside;
                        cx.notify();
                    }
                },
            ))
            .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, _, cx| {
                this.file_drag_active = false;
                let paths = paths.paths().to_vec();
                this.composer
                    .update(cx, |composer, cx| composer.add_paths(paths, cx));
                cx.notify();
            }))
            .child(
                // The conversation fades out at its bottom edge instead of
                // hard-cutting against the composer — a gradient overlay from
                // transparent into the panel background.
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(outlet)
                    .child(
                        div()
                            .absolute()
                            .bottom_0()
                            .left_0()
                            .right(px(10.0))
                            .h(px(40.0))
                            .bg(gpui::linear_gradient(
                                0.0,
                                gpui::linear_color_stop(theme_bg, 0.0),
                                gpui::linear_color_stop(theme_bg.opacity(0.0), 1.0),
                            )),
                    )
                    .children(self.render_jump_to_bottom(cx)),
            )
            // Reserved status strip (h-6) — the WorkingIndicator lives here so
            // the composer below never shifts. Both live INSIDE the
            // conversation region, ABOVE the terminal dock (comet __root.tsx:
            // the terminal panel sits below the whole conversation column).
            .child(status)
            // On the review route, the payload waits above the composer in the
            // column it will be delivered into (review.md I4).
            .children(self.render_delivery_preview(theme, cx))
            .when(has_spaces, |el| el.child(self.composer.clone()))
            .when(with_terminal, |el| {
                el.child(self.render_terminal_container(cx))
            })
            .when(file_drag_active, |el| {
                el.child(
                    div()
                        .absolute()
                        .inset_0()
                        .bg(gpui::hsla(0.0, 0.0, 0.0, 0.4))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(Theme::TEXT_BODY))
                        .text_color(theme.text)
                        .child("Drop images to attach"),
                )
            })
            .into_any_element()
    }

    /// The "↓ Scroll to bottom" pill (round-9 §3): a LABELED rounded-full
    /// chip — down-arrow glyph + 13px label on a near-opaque raised surface
    /// with a hairline — horizontally centered over the transcript column and
    /// floating a small gap above the composer. It hangs 14px below the
    /// conversation region (through the reserved h-6 status strip, whose
    /// content is left-aligned) so its bottom edge sits ~10px above the pill.
    /// Shown past the transcript's 320px threshold; 180ms fade + 2px rise in.
    fn render_jump_to_bottom(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.transcript.read(cx).jump_button_shown() {
            return None;
        }
        let theme = Theme::of(cx);
        Some(
            div()
                .absolute()
                .bottom(px(-14.0))
                .left_0()
                .right(px(10.0))
                .flex()
                .justify_center()
                .child(motion::dialog_in(
                    "jump-to-bottom",
                    div()
                        .id("jump-to-bottom-btn")
                        .h(px(30.0))
                        .rounded(px(Theme::RADIUS_ROW))
                        .border_1()
                        .border_color(theme.border)
                        .shadow_md()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .pl(px(11.0))
                        .pr(px(13.0))
                        .cursor_pointer()
                        // Hover must BRIGHTEN the opaque pill, never replace it
                        // with a translucent wash (a 10%-alpha bg here made the
                        // pill go see-through on hover — user-reported), and it
                        // fades over the CSS transition-colors 150ms, not snaps.
                        .bg(motion::hover_blend(
                            "jump-pill",
                            theme.surface_raised,
                            theme.surface_raised_hover,
                        ))
                        .on_hover(motion::hover_listener("jump-pill"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.transcript
                                .update(cx, |transcript, cx| transcript.jump_to_bottom(cx));
                        }))
                        .child(
                            div()
                                .text_size(px(Theme::TEXT_BODY))
                                .text_color(theme.text_muted)
                                .child(SharedString::from("↓")),
                        )
                        .child(
                            div()
                                .text_size(px(Theme::TEXT_BODY))
                                .text_color(theme.text)
                                .child(SharedString::from("Scroll to bottom")),
                        ),
                ))
                .into_any_element(),
        )
    }

    /// Terminal panel dock at the main-column bottom: a 5px height-drag handle
    /// over the panel, the whole container height-animated 200 ms on toggle.
    fn render_terminal_container(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let target = self.terminal_target(cx);
        let tween = self.terminal_tween;
        if target <= 0.0 && tween.is_none() {
            return gpui::Empty.into_any_element();
        }
        // Defensive: an open flag needs its entity (and set_open) even if
        // toggle_terminal never created one.
        if self.terminal_open(cx) && self.terminal.is_none() {
            let panel = self.terminal_panel(cx);
            panel.update(cx, |panel, cx| panel.set_open(true, cx));
        }
        let Some(panel) = self.terminal.clone() else {
            return gpui::Empty.into_any_element();
        };
        let border = Theme::of(cx).border;
        let handle_hover = Theme::of(cx).border_strong;
        let height = self.settings.terminal_height;

        let handle = div()
            .id("terminal-resize")
            .h(px(5.0))
            .w_full()
            .flex_none()
            .cursor_row_resize()
            .hover(move |s| s.bg(handle_hover))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, _| {
                    this.terminal_drag_anchor =
                        Some((f32::from(event.position.y), this.settings.terminal_height));
                }),
            )
            .on_drag(TerminalResize, |_, _point: Point<gpui::Pixels>, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DragGhost)
            })
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _, cx| {
                    if event.click_count == 2 {
                        this.settings.terminal_height = TERMINAL_DEFAULT_HEIGHT;
                        this.schedule_save(cx);
                        cx.notify();
                    }
                }),
            );

        // Fixed-height inner clipped by the animated container: content never
        // reflows mid-transition (same trick as the side panes).
        let inner = div()
            .h(px(height))
            .w_full()
            .flex()
            .flex_col()
            .child(handle)
            .child(div().flex_1().min_h_0().child(panel));

        div()
            .w_full()
            .flex_none()
            .overflow_hidden()
            .border_t_1()
            .border_color(border)
            .h(px(self.eval_tween(tween, target)))
            .child(inner)
            .into_any_element()
    }

    /// Working indicator strip: gradient spinner + rotating flavour word (7s,
    /// seeded per chat) + elapsed, staleness-gated via [`Indicator`]; falls back
    /// to a "Sending…" bridge and then the engine mode line.
    fn render_status_strip(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        let state = self.state.read(cx);

        // Aligned with the composer column: centered, same max width, small
        // inner gutter (comet's `mx-auto h-6 max-w-3xl px-2`).
        let strip = div()
            .h(px(Theme::STATUS_STRIP_HEIGHT))
            .flex_none()
            .w_full()
            .max_w(px(768.0))
            .mx_auto()
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .px(px(Theme::SPACE_LG + 8.0))
            .text_size(px(Theme::TEXT_CAPTION));

        let Some(chat_id) = state.selected_chat.clone() else {
            return strip.into_any_element();
        };
        let indicator = state.indicator_for(&chat_id, now);
        let session = state.session_for(&chat_id);
        let activity = session.and_then(|s| s.activity.clone());
        let waiting = activity
            .as_ref()
            .is_some_and(|a| comet_proto::view::activity_is_waiting(a, now));
        let elapsed_secs = session
            .and_then(|s| s.started_at)
            .map(|t| now.signed_duration_since(t).num_seconds())
            .unwrap_or(0);
        let sending = self.composer.read(cx).is_sending();

        match indicator {
            Indicator::Working if waiting => {
                // Waiting beats silence (gh#605): once one call has held past
                // the threshold, the strip names it instead of rotating a
                // flavour word — "running `cargo test` · 4m12s" is a sentence
                // about THIS run, and the warning tone says the wait is no
                // longer ordinary.
                let label = comet_proto::view::activity_label(
                    activity.as_ref().expect("checked above"),
                    now,
                );
                strip
                    .child(loaders::gradient_spinner(&theme, 2.5, cx.entity_id(), cx))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(Theme::TEXT_DENSE))
                            .text_color(theme.warning_text())
                            .child(SharedString::from(label)),
                    )
                    .into_any_element()
            }
            Indicator::Working => {
                let word =
                    transcript::flavour_word(transcript::flavour_seed(&chat_id), elapsed_secs);
                strip
                    .child(loaders::gradient_spinner(&theme, 2.5, cx.entity_id(), cx))
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_DENSE))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(format!("{word}…"))),
                    )
                    .child(
                        div()
                            .text_color(theme.text_subtle)
                            .child(SharedString::from(transcript::format_elapsed(elapsed_secs))),
                    )
                    .into_any_element()
            }
            // No label: the QuestionPanel right below IS the awaiting-input
            // surface — a strip caption above it was redundant (user request).
            Indicator::AwaitingInput => strip.into_any_element(),
            Indicator::Errored => strip
                .text_color(theme.danger)
                .child(SharedString::from("Run failed"))
                .into_any_element(),
            Indicator::None if sending => strip
                .child(loaders::gradient_spinner(&theme, 2.5, cx.entity_id(), cx))
                .child(
                    div()
                        .text_size(px(Theme::TEXT_DENSE))
                        .text_color(theme.text_muted)
                        .child(SharedString::from("Sending…")),
                )
                .into_any_element(),
            Indicator::None => strip.into_any_element(),
        }
    }

    /// Right dock — hidden by default, drag-resizable; content is either the
    /// task board (global) or the lazy [`Changes`] diff viewer. They share the
    /// slot and the width; toggling one closes the other.
    fn render_right_pane(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let bg = theme.bg;
        let content: AnyElement = if self.board_open {
            let panel = self.board.clone();
            // Idempotent — also covers a toggle that landed before the engine
            // finished booting.
            panel.update(cx, |panel, cx| panel.set_open(true, cx));
            panel.into_any_element()
        } else if self.right_pane_open(cx) {
            let changes = self.changes_pane(cx);
            // Idempotent — also covers a persisted-open pane on boot.
            changes.update(cx, |changes, cx| changes.ensure_watch(cx));
            changes.into_any_element()
        } else {
            gpui::Empty.into_any_element()
        };
        // Its OWN inset card (user request): the conversation card's right
        // gutter is the gap; padding (not margins) keeps the tweened width
        // container clean, and the resize grabber floats over the gap.
        let handle = self
            .resize_handle(
                "right-pane-resize",
                || RightPaneResize,
                |shell, _| shell.settings.right_pane_width = RIGHT_PANE_DEFAULT,
                cx,
            )
            .absolute()
            .top_0()
            .bottom_0()
            // INSIDE the width-clipped container (a negative inset was
            // clipped into unreachability — user-reported dead resize),
            // overlapping the card's left border.
            .left(px(0.0));
        let card = div()
            .size_full()
            .rounded(px(Theme::RADIUS_CARD))
            .border_1()
            .border_color(theme.border)
            .bg(bg)
            .overflow_hidden()
            .child(content);
        let target = self.right_target(cx);
        self.pane_container(
            self.right_tween,
            target,
            // Mirrors the conversation card's box exactly: flush under the
            // titlebar (no top pad), 8px bottom/right gutters — the
            // conversation card's own right margin is the 8px gap between the
            // two insets (user-reported height/gap mismatch).
            div()
                .h_full()
                .relative()
                .pb(px(8.0))
                .pr(px(8.0))
                .child(card)
                .child(handle)
                .into_any_element(),
        )
    }

    fn render_gate_card(&mut self, phase: &GatePhase, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let content: AnyElement = match phase {
            // Backend unreachable: quiet centered copy (comet Gate `Failed`),
            // plus a Retry affordance (the native engine doesn't self-redial).
            GatePhase::Failed(error) => div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(Theme::SPACE_MD))
                .child(
                    div()
                        .text_size(px(Theme::TEXT_BODY))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(error.clone())),
                )
                .child(
                    div()
                        .id("retry-engine")
                        .px(px(12.0))
                        .py(px(6.0))
                        .rounded(px(Theme::RADIUS_CHIP))
                        .border_1()
                        .border_color(theme.border)
                        .text_size(px(Theme::TEXT_BODY))
                        .text_color(theme.text)
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.element_hover))
                        .on_click(cx.listener(|this, _, _, cx| this.retry_engine(cx)))
                        .child(SharedString::from("Retry")),
                )
                .into_any_element(),
            // Login card (comet App.tsx Gate): centered card on the grid —
            // logo, "Log in to Comet", copy, full-width white Log in button.
            _ => div()
                .w(px(360.0))
                .px(px(32.0))
                .py(px(40.0))
                .rounded(px(Theme::RADIUS_CARD))
                .border_1()
                .border_color(theme.border)
                .bg(theme.card)
                .shadow(theme.float_shadow())
                .flex()
                .flex_col()
                .items_center()
                .text_center()
                .child(
                    icon(icons::COMET_LOGO)
                        .w(px(31.4))
                        .h(px(36.0))
                        .text_color(theme.text),
                )
                .child(
                    div()
                        .mt(px(24.0))
                        .text_size(px(Theme::TEXT_TITLE))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.text)
                        .child(SharedString::from("Log in to Comet")),
                )
                .child(
                    div()
                        .mt(px(6.0))
                        .mb(px(24.0))
                        .text_size(px(Theme::TEXT_BODY))
                        .line_height(px(19.0))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(
                            "This opens your browser to finish logging in — you'll come right back.",
                        )),
                )
                .child(
                    div()
                        .id("sign-in")
                        .w_full()
                        .h(px(36.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(Theme::RADIUS_ROW))
                        .bg(theme.text)
                        .text_size(px(Theme::TEXT_BODY))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.bg)
                        .cursor_pointer()
                        .hover(|s| s.opacity(0.9))
                        .on_click(cx.listener(|this, _, _, cx| this.start_sign_in(cx)))
                        .child(SharedString::from("Log in")),
                )
                .into_any_element(),
        };
        div()
            .size_full()
            .relative()
            .bg(theme.bg)
            .child(grid_backdrop(&theme))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    // Keyed per phase (comet App.tsx `<div key={phase}
                    // className="animate-in">`): every gate swap replays the
                    // 0.5s entrance instead of mutating one animated element.
                    .child(motion::fade_in(
                        match phase {
                            GatePhase::SignIn => "gate-card-signin",
                            _ => "gate-card-failed",
                        },
                        div().child(content),
                    )),
            )
            .into_any_element()
    }

    /// The OrgGate ("Create your workspace"): name form + existing memberships
    /// + "Use a different account" (feature-inventory §1.2).
    fn render_org_gate(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.ensure_org_ui(cx);
        let theme = Theme::of(cx).clone();
        let Some(org) = self.org.as_ref() else {
            return Empty.into_any_element();
        };
        let submitting = org.submitting;
        let error = org.error.clone();
        let name_input = org.name_input.clone();
        let invite_input = org.invite_input.clone();
        let orgs = org.orgs.clone();

        let email: Option<SharedString> = self
            .state
            .read(cx)
            .auth_user()
            .map(|u| u.email.clone().into());

        let memberships: AnyElement =
            match &orgs {
                Loadable::Idle | Loadable::Loading => div()
                    .mt(px(24.0))
                    .child(popover::skeleton_rows(&theme, 2, cx.entity_id(), cx))
                    .into_any_element(),
                Loadable::Error(message) => div()
                    .mt(px(24.0))
                    .child(
                        popover::error_row(&theme, message).child(
                            div()
                                .id("orgs-retry")
                                .px(px(Theme::SPACE_SM))
                                .py(px(3.0))
                                .rounded(px(Theme::RADIUS_CHIP))
                                .border_1()
                                .border_color(theme.border)
                                .text_color(theme.text)
                                .cursor_pointer()
                                .hover(|s| s.bg(theme.element_hover))
                                .on_click(cx.listener(|this, _, _, cx| this.load_orgs(cx)))
                                .child(SharedString::from("Retry")),
                        ),
                    )
                    .into_any_element(),
                Loadable::Ready(rows) if rows.is_empty() => Empty.into_any_element(),
                Loadable::Ready(rows) => div()
                    .mt(px(24.0))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .pb(px(8.0))
                            .text_size(px(Theme::TEXT_CAPTION))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_subtle)
                            .child(SharedString::from(
                                "Or continue in a workspace you belong to",
                            )),
                    )
                    .child(div().flex().flex_col().gap(px(4.0)).children(
                        rows.iter().enumerate().map(|(ix, row)| {
                            let org_id = row.organization_id.clone();
                            div()
                                .id(("org-row", ix))
                                .px(px(12.0))
                                .py(px(8.0))
                                .rounded(px(Theme::RADIUS_ROW))
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.bg)
                                .text_size(px(Theme::TEXT_BODY))
                                .text_color(theme.text)
                                .when(submitting, |el| el.opacity(0.5))
                                .cursor_pointer()
                                .hover(|s| s.bg(theme.wash(0.11)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.select_org(org_id.clone(), cx);
                                }))
                                .child(SharedString::from(row.name.clone()))
                        }),
                    ))
                    .into_any_element(),
            };

        // Someone who was invited into an existing workspace has nothing to
        // create — they have a code (gh#76). The same 36px field/button pair
        // as the name form, one notch quieter.
        let join = div()
            .mt(px(24.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .pb(px(8.0))
                    .text_size(px(Theme::TEXT_CAPTION))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_subtle)
                    .child(SharedString::from(
                        "Or join a workspace you were invited to",
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h(px(36.0))
                            .flex()
                            .items_center()
                            .px(px(12.0))
                            .rounded(px(Theme::RADIUS_ROW))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.bg)
                            .text_size(px(Theme::TEXT_BODY))
                            .child(invite_input),
                    )
                    .child(
                        div()
                            .id("accept-invite")
                            .h(px(36.0))
                            .px(px(16.0))
                            .flex()
                            .items_center()
                            .rounded(px(Theme::RADIUS_ROW))
                            .border_1()
                            .border_color(theme.border)
                            .text_size(px(Theme::TEXT_BODY))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .when(submitting, |el| el.opacity(0.5))
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.element_hover))
                            .on_click(cx.listener(|this, _, _, cx| this.accept_invite(cx)))
                            .child(SharedString::from("Join")),
                    ),
            );

        // comet App.tsx OrgGate: w-400 card on the grid — logo, headline,
        // explainer (+ signed-in email), name form with a white Create button,
        // then existing memberships and the account escape hatch.
        let blurb: SharedString = match email {
            Some(email) => format!(
                "Comet is organized around workspaces — create one for yourself or your team. Signed in as {email}."
            )
            .into(),
            None => {
                "Comet is organized around workspaces — create one for yourself or your team."
                    .into()
            }
        };
        let card = div()
            .w(px(400.0))
            .px(px(32.0))
            .py(px(36.0))
            .rounded(px(Theme::RADIUS_CARD))
            .border_1()
            .border_color(theme.border)
            .bg(theme.card)
            .shadow(theme.float_shadow())
            .flex()
            .flex_col()
            .child(
                icon(icons::COMET_LOGO)
                    .w(px(24.4))
                    .h(px(28.0))
                    .text_color(theme.text),
            )
            .child(
                div()
                    .mt(px(20.0))
                    .text_size(px(Theme::TEXT_TITLE))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(SharedString::from("Create your workspace")),
            )
            .child(
                div()
                    .mt(px(6.0))
                    .mb(px(24.0))
                    .text_size(px(Theme::TEXT_BODY))
                    .line_height(px(19.0))
                    .text_color(theme.text_muted)
                    .child(blurb),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h(px(36.0))
                            .flex()
                            .items_center()
                            .px(px(12.0))
                            .rounded(px(Theme::RADIUS_ROW))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.bg)
                            .text_size(px(Theme::TEXT_BODY))
                            .child(name_input),
                    )
                    .child(
                        div()
                            .id("create-org")
                            .h(px(36.0))
                            .px(px(16.0))
                            .flex()
                            .items_center()
                            .rounded(px(Theme::RADIUS_ROW))
                            .bg(theme.text)
                            .text_size(px(Theme::TEXT_BODY))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.bg)
                            .when(submitting, |el| el.opacity(0.5))
                            .cursor_pointer()
                            .hover(|s| s.opacity(0.9))
                            .on_click(cx.listener(|this, _, _, cx| this.create_org(cx)))
                            .child(SharedString::from(if submitting {
                                "Creating…"
                            } else {
                                "Create"
                            })),
                    ),
            )
            .child(memberships)
            .child(join)
            .when_some(error, |el, message| {
                el.child(
                    div()
                        .mt(px(16.0))
                        .text_size(px(Theme::TEXT_DENSE))
                        .line_height(px(17.0))
                        .text_color(theme.danger_text())
                        .child(message),
                )
            })
            .child(
                div().mt(px(24.0)).flex().flex_row().child(
                    div()
                        .id("org-signout")
                        .text_size(px(Theme::TEXT_DENSE))
                        .text_color(theme.text_subtle)
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme.text))
                        .on_click(cx.listener(|this, _, _, cx| this.sign_out(cx)))
                        .child(SharedString::from("Use a different account")),
                ),
            );

        div()
            .size_full()
            .relative()
            .bg(theme.bg)
            .child(grid_backdrop(&theme))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(motion::fade_in("org-gate-card", card)),
            )
            .into_any_element()
    }
}

/// The sign-in gate's faint grid backdrop (comet styles.css `.bg-grid`):
/// 44px hairlines at white 3.5%, with the radial mask approximated by edge
/// gradients back into the page background (gpui has no mask-image).
fn grid_backdrop(theme: &Theme) -> AnyElement {
    let line = theme.white_alpha(0.035);
    let bg = theme.bg;
    const STEP: f32 = 44.0;
    const SPAN: f32 = 2640.0;
    let verticals = (1..(SPAN / STEP) as usize).map(|i| {
        div()
            .absolute()
            .left(px(i as f32 * STEP))
            .top_0()
            .bottom_0()
            .w(px(1.0))
            .bg(line)
    });
    let horizontals = (1..((SPAN * 0.75) / STEP) as usize).map(|i| {
        div()
            .absolute()
            .top(px(i as f32 * STEP))
            .left_0()
            .right_0()
            .h(px(1.0))
            .bg(line)
    });
    div()
        .absolute()
        .inset_0()
        .overflow_hidden()
        .children(verticals)
        .children(horizontals)
        // Mask approximation: fade the grid back into the background toward
        // the window edges (the original masks to an ellipse at 50% / 40%).
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .h(px(120.0))
                .bg(gpui::linear_gradient(
                    180.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .child(
            div()
                .absolute()
                .bottom_0()
                .left_0()
                .right_0()
                .h(px(260.0))
                .bg(gpui::linear_gradient(
                    0.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .w(px(200.0))
                .bg(gpui::linear_gradient(
                    90.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .right_0()
                .w(px(200.0))
                .bg(gpui::linear_gradient(
                    270.0,
                    gpui::linear_color_stop(bg, 0.0),
                    gpui::linear_color_stop(bg.opacity(0.0), 1.0),
                )),
        )
        .into_any_element()
}

/// A size-6 icon button for the titlebar strip (comet window-controls.tsx:
/// `grid size-6 place-items-center rounded-md text-muted-foreground`).
fn window_control_button(
    id: &'static str,
    icon_path: &'static str,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let muted = theme.text_muted;
    let fade_key = format!("window-control-{id}");
    div()
        .id(id)
        .size(px(24.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(Theme::RADIUS_CHIP))
        .cursor_pointer()
        // comet window-controls.tsx: `transition-colors` — the wash fades.
        .bg(motion::hover_blend(
            &fade_key,
            theme.wash(0.0),
            theme.element_hover,
        ))
        .on_hover(motion::hover_listener(fade_key))
        // Buttons in/over a titlebar drag strip must be EXCLUDED from the
        // strip's event surface entirely. `.occlude()` (gpui
        // `HitboxBehavior::BlockMouse`) makes the window hit-test STOP at the
        // button, so every `is_hovered`-guarded strip listener — the
        // mouse-down that arms the drag, the mouse-move that hands AppKit a
        // native drag session (`performWindowDragWithEvent:`, whose second
        // quick click zooms NATIVELY on macOS), and the `click_count == 2`
        // zoom handler — never fires with the pointer over a button. It also
        // removes the button's rect from the native Drag control-area
        // hit-test on Windows/Linux. The click-level stop_propagation is
        // zed's ButtonLike belt on top. Double-click on EMPTY strip space
        // still zooms — nothing occludes it there.
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_click(event, window, cx)
        })
        .child(icon(icon_path).size(px(16.0)).text_color(muted))
}

/// A titlebar history button (comet window-controls.tsx): enabled it is a
/// normal window-control button; disabled it dims to 35% opacity and ignores
/// the pointer (`disabled:pointer-events-none disabled:opacity-35`).
fn nav_history_button(
    id: &'static str,
    icon_path: &'static str,
    enabled: bool,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    if !enabled {
        return div()
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            // Even disabled it reads as a control — occlude so double-clicks
            // on it don't fall through to the titlebar strip's zoom handler.
            .occlude()
            .child(icon(icon_path).size(px(16.0)).text_color(theme.text_faint))
            .into_any_element();
    }
    window_control_button(id, icon_path, theme, on_click).into_any_element()
}

/// A size-7 icon button for the main-panel header (comet __root.tsx:
/// `grid size-7 place-items-center rounded-md text-muted-foreground`).
fn header_icon_button(
    id: &'static str,
    icon_path: &'static str,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let muted = theme.text_muted;
    let fade_key = format!("header-icon-{id}");
    div()
        .id(id)
        .size(px(28.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(Theme::RADIUS_CHIP))
        .cursor_pointer()
        // comet __root.tsx header buttons: `transition-colors`.
        .bg(motion::hover_blend(
            &fade_key,
            theme.wash(0.0),
            theme.wash(0.11),
        ))
        .on_hover(motion::hover_listener(fade_key))
        // Same occlusion + click-swallowing as [`window_control_button`]: this
        // button sits inside the chat header's titlebar drag region, so its
        // rect must be carved out of the strip's drag/double-click surface.
        .occlude()
        .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_click(event, window, cx)
        })
        .child(icon(icon_path).size(px(16.0)).text_color(muted))
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        // The shell tone (comet `.frost`): the surface the sidebar sits on and
        // the main panel floats over as an inset rounded card. On macOS the
        // window background is the blurred desktop (lib.rs `Blurred`), so the
        // frost paints translucent — the sidebar and card margins read as
        // glass while the opaque card keeps text off it.
        let (frost, text, font) = (theme.glass(), theme.text, theme.font_sans.clone());
        let gate = self
            .debug_gate
            .clone()
            .unwrap_or_else(|| self.state.read(cx).gate());

        // Fullscreen hides the macOS traffic lights — reflow the control
        // cluster with a 200ms ease-out tween (§1.1). A fullscreen transition
        // resizes the window, which re-renders us, so polling here is exact.
        let fullscreen = window.is_fullscreen();
        if self.fullscreen != Some(fullscreen) {
            if self.fullscreen.is_some() && cfg!(target_os = "macos") {
                self.titlebar_tween = Some(WidthTween::new(
                    titlebar_cluster_start(!fullscreen),
                    titlebar_cluster_start(fullscreen),
                ));
            }
            self.fullscreen = Some(fullscreen);
        }
        // Manual tween drive bookkeeping for this pass (see [`WidthTween`]).
        self.reduced_motion = motion::reduced_motion(cx);
        self.motion_active.set(false);

        // Keyboard shortcuts (mod-s/b/j) dispatch through the window focus
        // chain — with nothing focused they go dead. Land initial focus on the
        // composer, and whenever focus is lost with no successor (e.g. the
        // focused element unmounted), route it back there.
        if self.focus_sub.is_none() {
            self.focus_sub = Some(cx.on_focus_lost(window, |this: &mut Shell, window, cx| {
                match this.route {
                    // The review route mounts the same composer, in the column
                    // beside the card — so focus lands there for the same
                    // reason it does on a chat.
                    Route::Chat | Route::Review { .. } => {
                        window.focus(&this.composer.focus_handle(cx), cx)
                    }
                    // No composer here — clear the stale handle so `focused()`
                    // reads None (the render hook below re-lands focus when the
                    // route returns to Chat; a lingering unmounted handle would
                    // otherwise dead-end keyboard dispatch for good).
                    Route::Settings(_) => window.blur(),
                }
            }));
        }
        if matches!(gate, GatePhase::Ready)
            && matches!(self.route, Route::Chat | Route::Review { .. })
            && window.focused(cx).is_none()
        {
            window.focus(&self.composer.focus_handle(cx), cx);
        }
        if std::mem::take(&mut self.focus_composer_next_render) {
            window.focus(&self.composer.focus_handle(cx), cx);
        }

        let root = div()
            .id("shell-root")
            .relative()
            .flex()
            .flex_row()
            .size_full()
            .bg(frost)
            .text_color(text)
            .font_family(font)
            .text_size(px(Theme::TEXT_BODY))
            .on_drag_move(cx.listener(Self::on_sidebar_drag))
            .on_drag_move(cx.listener(Self::on_right_pane_drag))
            .on_drag_move(cx.listener(Self::on_terminal_drag))
            .on_drag_move(cx.listener(Self::on_review_session_drag))
            // GPUI exposes native key-repeat directly. Swallow only repeated
            // physical Cmd-T keydowns; the initial keydown continues to the
            // typed action binding, while menu and click actions are untouched.
            .capture_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if commands::suppress_repeated_shortcut(
                    &event.keystroke,
                    event.is_held,
                    &this.settings.keymap,
                ) {
                    cx.stop_propagation();
                    return;
                }
                if this.handle_new_session_chooser_key(&event.keystroke.key, cx) {
                    cx.stop_propagation();
                }
            }))
            // `esc` leaves a review (gh#311). The board panel says "esc close"
            // in its own footer and the review must not be the one surface in
            // this app where the key does nothing — a route you can only leave
            // by knowing a shortcut is a room without a door.
            //
            // A raw key rather than a binding: `escape` is deliberately unbound
            // in the composer keymaps, so it arrives here having passed every
            // surface that wanted it first (each of those stops propagation),
            // and a global keybinding would take it from all of them.
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape"
                    && !event.keystroke.modifiers.modified()
                    && matches!(this.route, Route::Review { .. })
                    && !this.overlay_open()
                {
                    cx.stop_propagation();
                    this.close_review(cx);
                }
            }))
            // The panel shortcuts are chat-scoped chrome: in Settings they are
            // no-ops (comet __root.tsx gates the hotkey on `!isSettings`, and
            // the terminal panel is only mounted on session routes). The
            // sidebar toggle stays live everywhere, as in the original.
            .on_action(cx.listener(|this, _: &ToggleTerminal, window, cx| {
                if matches!(this.route, Route::Chat) {
                    this.toggle_terminal(window, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| this.toggle_sidebar(cx)))
            .on_action(cx.listener(|this, _: &ToggleChanges, _, cx| {
                if matches!(this.route, Route::Chat) {
                    this.toggle_right_pane(cx)
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleBoard, window, cx| {
                this.toggle_board_from_route(window, cx)
            }))
            .on_action(cx.listener(|this, _: &AddSpacePalette, _, cx| {
                if this.add_space.is_some() {
                    this.add_space = None;
                    cx.notify();
                } else {
                    this.open_add_space(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NewSession, _, cx| this.new_session(None, cx)));

        let root = match &gate {
            GatePhase::Ready => {
                // A run finishing while you're LOOKING at the session must not
                // badge "completed" until you leave and return — mark it seen
                // live while the window is active (idempotent guard inside;
                // one extra frame settles it).
                if window.is_window_active() {
                    let unseen_selected = {
                        let s = self.state.read(cx);
                        s.selected_chat_row()
                            .filter(|c| c.unseen())
                            .map(|c| c.id.clone())
                    };
                    if let Some(chat_id) = unseen_selected {
                        self.state
                            .update(cx, |s, cx| s.mark_chat_seen(&chat_id, cx));
                    }
                }
                // Capture knob: `COMET_OPEN_DIALOG=model` pops the combined
                // harness/model menu (needs `window`, so it fires here rather
                // than in `on_state_changed`).
                if self.debug_dialog.as_deref() == Some("model") {
                    self.debug_dialog = None;
                    self.composer
                        .update(cx, |c, cx| c.debug_open_model_menu(window, cx));
                }
                // MessageRail width gate: hide below 48rem of main-panel width.
                // On the review route the transcript is the narrow column, not
                // the card — so the width that gates the rail is the column's,
                // and the whole window would be the wrong measure of it.
                let viewport = f32::from(window.viewport_size().width);
                let main_width = if matches!(self.route, Route::Review { .. }) {
                    self.settings
                        .review_session_width
                        .clamp(REVIEW_SESSION_MIN, REVIEW_SESSION_MAX)
                } else {
                    viewport - self.sidebar_target() - self.right_target(cx) - 10.0
                };
                self.transcript.update(cx, |t, cx| {
                    t.set_rail_enabled(rail::rail_visible(main_width), cx)
                });

                let sidebar = self.render_sidebar(cx);
                let sidebar_handle = self.resize_handle(
                    "sidebar-resize",
                    || SidebarResize,
                    |shell, _| shell.settings.sidebar_width = SIDEBAR_DEFAULT,
                    cx,
                );
                let main = self.render_main(cx);
                // The Changes pane is chat-scoped chrome: the Settings route
                // never renders it (comet __root.tsx `!isSettings && activeChat`
                // around the diff column) — the per-session open flags stay
                // intact for the return trip.
                let on_chat = matches!(self.route, Route::Chat);
                // The review route fills the same slot with the authoring
                // session (gh#276): the reference goes where every other
                // reference in this app goes.
                let on_review = matches!(self.route, Route::Review { .. });
                let right: AnyElement = if on_chat {
                    self.render_right_pane(cx)
                } else if on_review {
                    self.render_review_session(cx)
                } else {
                    Empty.into_any_element()
                };
                let overlays = self.render_overlays(window.viewport_size(), window, cx);
                // The signature frame: the conversation card and — when the
                // changes pane is open — a SECOND inset card beside it, both
                // rounded hairline-bordered floats on the frost shell (the
                // changes card is built inside `render_right_pane`).
                let theme = Theme::of(cx).clone();
                // Margins, radius, and border-color MELT over the same 200ms
                // ease-out as the sidebar width (comet __root.tsx `<main>`
                // `transition-[margin,border-radius,border-color]`; collapsed
                // is `m-0 rounded-none border-transparent` — the border WIDTH
                // stays, only its color fades, so layout never jumps by the
                // hairline).
                let border_color = theme.border;
                let card = div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .overflow_hidden()
                    .bg(theme.bg)
                    .border_1()
                    .child(main);
                // Manual drive on the SAME clock as the sidebar width tween.
                // Crucially there is no `with_animation` wrapper here: the
                // wrapper's epoch-keyed id used to change every card
                // descendant's global element-id path on each toggle, which
                // reset gpui's per-element animation state and REPLAYED any
                // stale pane/terminal tween from t=0 (the changes pane slid
                // ~100px under the clip mid-toggle — round-6 §2/§3).
                //
                // The inset card persists in EVERY state (user request): top
                // gutter under the unified titlebar, constant left/right/
                // bottom gutters, constant radius + hairline — the 8px left
                // gap holds whether it borders the sidebar or the window edge.
                // No top margin: the titlebar's own internal air (44px bar,
                // 28px tabs) is the gap — an extra gutter read as a hole
                // between the header and the app (user report).
                // The right margin is the window gutter when the changes
                // pane is closed, but the SEAM between the two inset cards
                // when it's open — a full gutter there read double-wide next
                // to the two borders it separates (user report).
                let right_gap = if (on_chat && self.right_slot_open(cx)) || on_review {
                    4.0
                } else {
                    8.0
                };
                let card: AnyElement = card
                    .mb(px(8.0))
                    .mr(px(right_gap))
                    .ml(px(8.0))
                    .rounded(px(Theme::RADIUS_CARD))
                    .border_color(border_color)
                    .into_any_element();
                // The whole app page is one keyed `animate-in` entrance (comet
                // App.tsx `<div key={phase} className="animate-in h-full">`):
                // arriving from the splash or any gate fades the page in; the
                // splash-out crossfades over it on boot.
                // The sidebar resize handle FLOATS over the sidebar/card seam
                // (zero layout width, same idiom as the changes-pane grabber)
                // so the sidebar's right gutter stays exactly as wide as its
                // left one — a 5px flex child here read as lopsided spacing.
                let sidebar_seam = div()
                    .w(px(0.0))
                    .h_full()
                    .flex_none()
                    .relative()
                    .child(sidebar_handle.absolute().top_0().bottom_0().left(px(-2.0)));
                let title_bar = self.render_title_bar(cx);
                // The sidebar column: a full-window-height band (under the
                // traffic lights, through the titlebar, down to the bottom
                // edge) whose only mark is the hairline on its right edge. Its
                // width rides the same tween as the sidebar, so the seam melts
                // away with the collapse instead of vanishing in a frame.
                //
                // It paints NO fill. The canvas separates the sidebar from the
                // shell with the 1px `--line` hairline and nothing else (claim
                // A3) — one tone on both sides. A `wash(0.05)` used to sit here
                // on top of the window's frost, a 13-point step in light
                // (218 against the shell's 231) that read as two panels; since
                // gh#293/#299 put the tab strip where the canvas puts it, at
                // x=172, the strip crossed that seam and the tabs looked like
                // they straddled it. The frost itself is untouched (root `.bg`
                // is [`Theme::glass`]) — the window stays translucent, and what
                // went is the internal step the canvas never had (gh#304).
                let sidebar_now = self.eval_tween(self.sidebar_tween, self.sidebar_target());
                let sidebar_column = div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left_0()
                    .w(px(sidebar_now))
                    .border_r_1()
                    .border_color(border_color);
                let page = div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(title_bar)
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .flex_row()
                            .child(sidebar)
                            .child(sidebar_seam)
                            .child(card)
                            .child(right),
                    )
                    .child(self.render_titlebar_cluster(cx))
                    .children(overlays);
                root.child(sidebar_column)
                    .child(motion::fade_in("phase-app", page))
            }
            GatePhase::Loading => root, // splash overlay covers boot
            GatePhase::OrgGate => {
                let card = self.render_org_gate(cx);
                root.child(card)
            }
            phase @ (GatePhase::Failed(_) | GatePhase::SignIn) => {
                let card = self.render_gate_card(phase, cx);
                root.child(card)
            }
        };

        // A manually-driven tween is mid-flight: keep frames coming (the same
        // scheduling `with_animation` would have requested). Hover color fades
        // ride the same clock; their once-per-frame tick lives here (this is
        // the window's root render — it runs exactly once per frame).
        if self.motion_active.get() | motion::hover_fades_active() {
            window.request_animation_frame();
        }

        // Boot splash overlay: visible → crossfades out on Ready → removed.
        match self.splash {
            SplashPhase::Visible => {
                let theme = Theme::of(cx).clone();
                let view = cx.entity_id();
                root.child(loaders::splash_overlay(&theme, false, view, cx))
            }
            SplashPhase::FadingOut => {
                let theme = Theme::of(cx).clone();
                let view = cx.entity_id();
                root.child(loaders::splash_overlay(&theme, true, view, cx))
            }
            SplashPhase::Gone => root,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The settings nav, in the order the supplied design file lists it
    /// (gh#258). Order is the only thing the rail communicates beyond the
    /// nine labels, so it is worth a test: it groups this device (Devices,
    /// Agents, Members), then this app (Appearance, Shortcuts), then the board
    /// — which may be on another machine entirely — and closes with Archived.
    #[test]
    fn the_settings_nav_is_in_the_designs_order() {
        assert_eq!(
            SettingsSection::ALL.map(SettingsSection::label),
            [
                "Devices",
                "Agents",
                "Members",
                "Appearance",
                "Shortcuts",
                "Routing",
                "Automations",
                "Stats",
                "Archived",
            ]
        );
    }

    #[test]
    fn titlebar_cluster_matches_comet_window_controls() {
        // comet window-controls.tsx: `left: fullscreen ? 12 : 88` — the
        // cluster clears the {14,15} traffic lights, and reclaims the inset
        // when fullscreen hides them.
        assert_eq!(titlebar_cluster_start(false), 88.0);
        assert_eq!(titlebar_cluster_start(true), 12.0);
    }

    #[test]
    fn titlebar_spacer_selects_per_platform_and_fullscreen() {
        // macOS, lights visible: spacer fills up to the 88px cluster start.
        assert_eq!(titlebar_spacer_width(true, false, 10.0), 78.0);
        assert_eq!(titlebar_spacer_width(true, false, 12.0), 76.0);
        assert_eq!(titlebar_spacer_width(true, false, 26.0), 62.0);
        // macOS fullscreen: the inset animates away (clamped at zero when the
        // strip's own padding already exceeds the 12px cluster start).
        assert_eq!(titlebar_spacer_width(true, true, 10.0), 2.0);
        assert_eq!(titlebar_spacer_width(true, true, 26.0), 0.0);
        // Linux / Windows: never any inset.
        assert_eq!(titlebar_spacer_width(false, false, 10.0), 0.0);
        assert_eq!(titlebar_spacer_width(false, true, 10.0), 0.0);
    }

    #[test]
    fn cluster_clearance_clears_the_overlay_buttons() {
        // Linux: buttons at 10..86; a 16px-padded header needs 78 more px to
        // put content at 86 + 8 breathing room.
        assert_eq!(cluster_clearance(false, false, 16.0), 78.0);
        assert_eq!(cluster_clearance(false, false, 10.0), 84.0);
        // macOS: buttons start at the 88px traffic-light cluster start.
        assert_eq!(
            cluster_clearance(true, false, 16.0),
            88.0 + 76.0 + 8.0 - 16.0
        );
        // macOS fullscreen: cluster reclaims the inset (starts at 12).
        assert_eq!(
            cluster_clearance(true, true, 16.0),
            12.0 + 76.0 + 8.0 - 16.0
        );
    }

    // ---- per-session panel flags (§1.10/1.11 parity: comet sessionPanels) ----

    #[test]
    fn session_panels_default_closed_per_chat() {
        let panels = SessionPanels::default();
        assert_eq!(panels.get("a"), ChatPanels::default());
        assert!(!panels.get("a").terminal_open);
        assert!(!panels.get("a").changes_open);
        // The new-chat canvas ("" key) is its own session, also closed.
        assert!(!panels.get("").terminal_open);
    }

    #[test]
    fn session_panels_flags_are_chat_scoped() {
        let mut panels = SessionPanels::default();
        // Opening the terminal in chat A opens it ONLY in chat A.
        assert!(panels.toggle_terminal("a"));
        assert!(panels.get("a").terminal_open);
        assert!(!panels.get("b").terminal_open);
        assert!(!panels.get("").terminal_open);
        // Changes pane in B is independent of A's terminal.
        assert!(panels.toggle_changes("b"));
        assert!(panels.get("b").changes_open);
        assert!(!panels.get("b").terminal_open);
        assert!(!panels.get("a").changes_open);
        // Switching back to A restores A's state untouched.
        assert!(panels.get("a").terminal_open);
        // Toggling off round-trips.
        assert!(!panels.toggle_terminal("a"));
        assert!(!panels.get("a").terminal_open);
    }

    #[test]
    fn session_panels_both_flags_coexist_per_chat() {
        let mut panels = SessionPanels::default();
        panels.toggle_terminal("a");
        panels.toggle_changes("a");
        assert_eq!(
            panels.get("a"),
            ChatPanels {
                terminal_open: true,
                changes_open: true
            }
        );
        assert_eq!(panels.get("b"), ChatPanels::default());
    }

    // ---- navigation history (titlebar back/forward) ----

    fn chat(id: &str) -> NavEntry {
        NavEntry::Chat(id.to_string())
    }

    #[test]
    fn nav_history_starts_with_nothing_to_walk() {
        let nav = NavHistory::new(chat(""));
        assert!(!nav.can_back());
        assert!(!nav.can_forward());
        assert_eq!(*nav.current(), chat(""));
    }

    #[test]
    fn nav_push_then_back_and_forward() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(chat("b"));
        nav.push(NavEntry::Settings(SettingsSection::Devices));
        assert!(nav.can_back());
        assert!(!nav.can_forward());

        // Back walks toward the oldest entry without dropping anything.
        assert_eq!(
            nav.back(),
            Some(chat("b")),
            "back lands on the previous route"
        );
        assert_eq!(nav.back(), Some(chat("a")));
        assert!(!nav.can_back());
        assert!(nav.can_forward());
        assert_eq!(nav.back(), None, "past the oldest entry is a no-op");

        // Forward retraces the same path.
        assert_eq!(nav.forward(), Some(chat("b")));
        assert_eq!(
            nav.forward(),
            Some(NavEntry::Settings(SettingsSection::Devices))
        );
        assert!(!nav.can_forward());
        assert_eq!(nav.forward(), None);
    }

    #[test]
    fn nav_push_dedups_the_current_route() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(chat("a"));
        nav.push(chat("a"));
        assert_eq!(nav.len(), 1, "re-selecting the current route never stacks");
        nav.push(NavEntry::Settings(SettingsSection::Agents));
        nav.push(NavEntry::Settings(SettingsSection::Agents));
        assert_eq!(nav.len(), 2);
    }

    #[test]
    fn nav_push_truncates_the_forward_branch() {
        // a → b → c, back to a, then push d: the b/c branch is gone (browser
        // semantics — comet's memory history PUSH truncates entries ahead).
        let mut nav = NavHistory::new(chat("a"));
        nav.push(chat("b"));
        nav.push(chat("c"));
        nav.back();
        nav.back();
        assert_eq!(*nav.current(), chat("a"));
        assert!(nav.can_forward());
        nav.push(chat("d"));
        assert!(!nav.can_forward(), "the old branch is unreachable");
        assert_eq!(nav.len(), 2);
        assert_eq!(nav.back(), Some(chat("a")));
        assert_eq!(nav.forward(), Some(chat("d")));
    }

    #[test]
    fn nav_replace_swaps_in_place() {
        // The boot auto-select replaces the untouched canvas entry, so Back
        // stays disabled after landing in the last-used chat.
        let mut nav = NavHistory::new(chat(""));
        nav.replace(chat("boot"));
        assert_eq!(nav.len(), 1);
        assert_eq!(*nav.current(), chat("boot"));
        assert!(!nav.can_back());
    }

    #[test]
    fn nav_settings_sections_are_distinct_entries() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(NavEntry::Settings(SettingsSection::Devices));
        nav.push(NavEntry::Settings(SettingsSection::Shortcuts));
        assert_eq!(nav.len(), 3, "section changes are navigations");
        assert_eq!(
            nav.back(),
            Some(NavEntry::Settings(SettingsSection::Devices))
        );
        assert_eq!(nav.back(), Some(chat("a")));
    }

    // ---- the review route (gh#180) ----------------------------------------

    fn review(task: &str, chat: Option<&str>) -> NavEntry {
        NavEntry::Review {
            task_id: task.to_string(),
            chat_id: chat.map(str::to_string),
        }
    }

    /// A review is a route with a subject, so Back has to land on the pairing
    /// it left — the same attempt AND the same session beside it — rather than
    /// on whatever is selected by the time you press it.
    #[test]
    fn a_review_navigates_as_one_pairing_of_attempt_and_session() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(review("gh:o/r#180", Some("chat-1")));
        nav.push(review("gh:o/r#183", Some("chat-2")));
        assert_eq!(nav.len(), 3, "two reviews are two navigations");
        assert_eq!(nav.back(), Some(review("gh:o/r#180", Some("chat-1"))));
        assert_eq!(nav.back(), Some(chat("a")));
        assert_eq!(nav.forward(), Some(review("gh:o/r#180", Some("chat-1"))));
    }

    /// Re-opening the review you are already on is not a navigation — the same
    /// dedup every other route gets, and the one that stops `r` on the board
    /// from stacking history while somebody re-reads a row.
    #[test]
    fn reopening_the_same_review_does_not_stack_history() {
        let mut nav = NavHistory::new(chat("a"));
        nav.push(review("gh:o/r#180", Some("chat-1")));
        nav.push(review("gh:o/r#180", Some("chat-1")));
        assert_eq!(nav.len(), 2);
        // The same attempt reviewed with its chat gone IS a different entry:
        // the column beside the card is not the same column.
        nav.push(review("gh:o/r#180", None));
        assert_eq!(nav.len(), 3);
    }

    /// The inversion, as a number: the session column on the review route is
    /// narrower than the Changes dock it swaps roles with, because it is the
    /// reference now and the review is the content.
    #[test]
    fn the_review_session_column_is_a_reference_width_not_a_content_width() {
        assert!(REVIEW_SESSION_DEFAULT < RIGHT_PANE_DEFAULT);
        assert!(REVIEW_SESSION_MIN < REVIEW_SESSION_DEFAULT);
        assert!(REVIEW_SESSION_DEFAULT < REVIEW_SESSION_MAX);
        // Persisted like every other pane width, and healed into range on load
        // — a hand-edited file must not be able to hide the review behind its
        // own reference column.
        let wide = UiSettings {
            review_session_width: 4000.0,
            ..UiSettings::default()
        }
        .clamped();
        assert_eq!(wide.review_session_width, REVIEW_SESSION_MAX);
        let broken = UiSettings {
            review_session_width: f32::NAN,
            ..UiSettings::default()
        }
        .clamped();
        assert_eq!(broken.review_session_width, REVIEW_SESSION_DEFAULT);
    }
}

#[cfg(test)]
mod interaction_tests;
