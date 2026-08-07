//! Spaces sidebar: repo-named space rows that disclose their sessions inline
//! (gh#124), and the add-space palette (⌘K).
//!
//! A space = a synced (device, folder) pair; the sidebar's job is switching
//! between them and surfacing which sessions want attention. Child module of
//! `shell` so it renders straight off `Shell`'s private state.
//!
//! ## One session surface (gh#124)
//!
//! The sidebar is the authoritative enumeration of sessions: each space row
//! contains its sessions, disclosed inline under the row (click activates and
//! disclosing follows; the chevron toggles without activating). The old global
//! "Sessions" flat list is retired — it was a second session switcher
//! competing with the tab strip, and neither was authoritative. The tab strip
//! stays as in-space navigation with the titlebar duties.
//!
//! ## One full row per chat (gh#138)
//!
//! Active and the tree still answer different questions, but only one of them
//! draws a chat at a time: **Active owns it while its session is live, the
//! space's shelf shows it when idle.** Otherwise a box whose whole load sits
//! in one space renders that space as a verbatim copy of the list above it.
//! Two things keep the surfaces tied — the space row's `· 3 running` count,
//! and the shelf's own note when Active holds every one of its sessions
//! ([`spaces_view::space_shelf`]).
//!
//! A space row is named repo-first: `owner/repo` where a host has supplied the
//! gh#118 link ([`Shell::refresh_space_slugs`]), the folder basename otherwise.
//! The device's name appears ONCE, as a group header above the spaces it
//! hosts — not repeated on every row ([`spaces_view::device_groups`]).
//!
//! ## The palette has two doors (gh#118)
//!
//! It used to have one, and it was the wrong one: a folder browser asks "which
//! folder on which machine?", which is a question about infrastructure. Work
//! lives in GitHub repos and the box is where they run, so the front door is a
//! repo list — every space you have, named by its repo, plus every repo the
//! board's App can see that has no space yet. Picking a connected repo opens its
//! space wherever it lives; picking an unconnected one clones it onto the box
//! and lands you in the result, without the words "space" or "host" appearing.
//!
//! The folder browser is still there as the second door
//! ([`AddSpaceMode::Folders`]): a scratch directory that is nobody's repo is a
//! real place to work, and the repo list cannot offer it.

use super::*;
use crate::motion::TAB_SLIDE;
use crate::pickers::{breadcrumbs, browser_rows, parent_path};
use crate::terminal::panel::{drop_index, reorder_tabs, slide_offset};
use chrono::DateTime;
use comet_proto::view::board;
use comet_proto::view::needs::{self as needs_view};
use comet_proto::view::repos::{self, RepoOffer, RepoRow, SpaceSlug};
use comet_proto::view::spaces as spaces_view;
use comet_proto::{ChatIndicator, Device, FolderListing, Space};
use gpui::FocusHandle;

/// What [`methods::LIST_REPO_SPACES`] answers with (gh#118).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepoSpacesReply {
    /// The host's own device id — what a space's `device_id` is compared
    /// against. Reported rather than inferred: a call with no `targetDeviceId`
    /// went to this device, whose id the caller knows, but a call that was
    /// forwarded came back from a device whose id it should not have to guess.
    device_id: String,
    #[serde(default)]
    spaces: Vec<SpaceSlug>,
    #[serde(default)]
    repos: Vec<comet_board::onboard::Candidate>,
    #[serde(default)]
    repos_note: Option<String>,
}

/// Space-row slot height for drag drop-index math: py(6)×2 + 17px line ≈ 29,
/// plus the 2px column gap.
const SPACE_ROW_SLOT: f32 = 31.0;

/// Drag-reorder state for the spaces list; `epoch` keys the 150ms slide
/// animation restarts (the session-tab idiom, vertical).
///
/// Device-scoped (gh#124): a space cannot change its device by being dragged,
/// so a drag lives entirely inside one device group and the indices are
/// group-local. While any drag is live the nested session rows collapse, so
/// the drop-index math stays uniform ([`SPACE_ROW_SLOT`]).
pub(super) struct SpaceDragState {
    device: String,
    from: usize,
    over: usize,
    epoch: usize,
    prev_over: usize,
}

/// The dragged-row payload (gpui drag-and-drop), device-group-scoped.
struct SpaceDragPayload {
    device: String,
    from: usize,
    name: SharedString,
    repo: bool,
}

/// The floating row rendered at the cursor while dragging.
struct SpaceGhost {
    name: SharedString,
    repo: bool,
}

impl Render for SpaceGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .w(px(200.0))
            .h(px(29.0))
            .px(px(Theme::SPACE_SM))
            .flex()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .rounded(px(8.0))
            .bg(theme.surface_raised)
            .border_1()
            .border_color(theme.border_strong)
            .text_size(px(13.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.text)
            .opacity(0.85)
            .child(
                icon(if self.repo {
                    icons::GITHUB_MARK
                } else {
                    icons::FOLDER
                })
                .size(px(16.0))
                .flex_none()
                .text_color(theme.text_muted),
            )
            .child(div().truncate().child(self.name.clone()))
    }
}

/// Which of the palette's two doors is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AddSpaceMode {
    /// The front door (gh#118): repos, connected and not.
    Repos,
    /// The folder browser — this device (or any other), for the folders that
    /// are nobody's GitHub repo.
    Folders,
}

/// One device that answered [`methods::LIST_REPO_SPACES`], i.e. one board host.
///
/// The sweep collects every answer rather than stopping at the first, because
/// "how many boards are there?" is the question that decides whether onboarding
/// gets to pick a target silently. One host → don't ask. Two → ask.
struct RepoHost {
    /// Relay target: `None` is this device (no `targetDeviceId` passthrough).
    target: Option<String>,
    /// The device id the host reported for itself — what a space's `device_id`
    /// is compared against, and not derivable from `target` when it is `None`.
    device_id: String,
    links: Vec<SpaceSlug>,
    offers: Vec<RepoOffer>,
    /// Why the host offered no repos, when it offered none. Not an error: a
    /// board on a `GITHUB_TOKEN` has no App installations to enumerate, and its
    /// spaces are still spaces.
    note: Option<SharedString>,
}

/// The add-space palette (a command-K surface, summoned by ⌘K): search bar
/// across the top, the repo list (or the folder browser) on the left, a rail on
/// the right, kbd-hint footer. One surface — switching doors or picking a device
/// re-lists in place, no step wizard.
pub(super) struct AddSpaceFlow {
    mode: AddSpaceMode,
    /// The board hosts the sweep found. Empty after a completed sweep = no
    /// device here hosts a board, which makes the repo list the spaces alone.
    hosts: Vec<RepoHost>,
    /// Which host an onboard clones onto — an index into `hosts`. Only ever
    /// asked about when there is more than one.
    target: usize,
    /// The sweep's state; `Ready(())` once it has finished, however few hosts
    /// answered.
    repos: Loadable<()>,
    /// The slug currently being cloned on the box. The palette stays open around
    /// it: an onboard is a `git clone` on another machine, and a picker that
    /// vanished would leave its refusals nowhere to land.
    onboarding: Option<SharedString>,
    /// The device currently browsed (the highlighted rail row).
    device: Option<Device>,
    /// Filter input; Enter descends into the highlighted folder.
    search: Entity<ComposerInput>,
    browser: Loadable<FolderListing>,
    /// Requested browser path (`None` = the device's default, i.e. home).
    browser_path: Option<String>,
    /// The device's home (the path a `None` browse resolved to) — breadcrumbs
    /// fold everything up to here into the device-name crumb.
    home: Option<String>,
    /// Best-effort git seed for the CURRENT browser path (known when we
    /// descended through an entry whose `is_repo` we saw; the owning device's
    /// SpacesSync re-verifies either way).
    browser_repo: bool,
    /// Keyboard highlight within the FILTERED rows of whichever list is open.
    active: usize,
    submit_busy: bool,
    error: Option<SharedString>,
    /// Tracked on the card (`track_focus`) — puts the card on the keyboard
    /// dispatch path so ↑↓/⌫/esc reach `add_space_key` while the search input
    /// holds focus (the structure every working picker uses).
    focus: FocusHandle,
    /// Folder-list scroll — keyboard navigation keeps the highlighted row in
    /// view (`scroll_to_item`).
    list_scroll: gpui::ScrollHandle,
    focus_pending: bool,
    load_task: Option<Task<()>>,
    submit_task: Option<Task<()>>,
    /// The sweep, and then the onboard. Its own slot: dropping a gpui `Task`
    /// cancels it, and a folder browse landing mid-clone must not look like a
    /// cancelled clone (the discipline the routing page already keeps).
    repo_task: Option<Task<()>>,
    _search_events: Subscription,
}

impl AddSpaceFlow {
    /// The board host an onboard clones onto, if the sweep found one.
    fn host(&self) -> Option<&RepoHost> {
        self.hosts.get(self.target).or_else(|| self.hosts.first())
    }

    /// Every host's `space → repo` links, merged. Space ids are unique across
    /// devices, so concatenating cannot double-name one space.
    fn links(&self) -> Vec<SpaceSlug> {
        self.hosts.iter().flat_map(|h| h.links.clone()).collect()
    }

    /// Every host's App grant, merged and deduplicated by slug: two boards in
    /// one org can be installed on the same repo, and it is still one repo.
    fn offers(&self) -> Vec<RepoOffer> {
        let mut out: Vec<RepoOffer> = Vec::new();
        for offer in self.hosts.iter().flat_map(|h| h.offers.iter()) {
            if !out.iter().any(|o| o.slug.eq_ignore_ascii_case(&offer.slug)) {
                out.push(offer.clone());
            }
        }
        out
    }
}

/// The space-row Rename dialog (same shape as [`RenameChatDialog`]).
pub(super) struct RenameSpaceDialog {
    pub space_id: String,
    pub input: Entity<ComposerInput>,
    pub focus_pending: bool,
    pub _events: Subscription,
}

/// Dot color for a chat's display status (tab dots + Sessions rows).
pub(super) fn status_dot_color(status: ChatIndicator, theme: &Theme) -> gpui::Hsla {
    match status {
        // Pink, not amber — the harsh yellow read as a warning; running is
        // routine (user request).
        ChatIndicator::Working => {
            crate::theme::oklch(0.718, 0.202, 349.761).opacity(0.85) // pink-400
        }
        // Blue: "asking you a question" must read differently from "busy
        // working" at a glance.
        ChatIndicator::AwaitingInput => theme.accent.opacity(0.9),
        ChatIndicator::Errored => theme.danger,
        // Green: finished-but-unseen reads as "ready for you".
        ChatIndicator::Completed => {
            crate::theme::oklch(0.765, 0.177, 163.223).opacity(0.9) // emerald-400
        }
        ChatIndicator::Idle => theme.white_alpha(0.14),
    }
}

impl Shell {
    // ---- space switching ----

    /// Land in a space: remembered tab if alive, else the most recent chat in
    /// the space, else the new-session canvas. Persists `last_space_id`.
    pub(super) fn activate_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.route = Route::Chat;
        self.state.update(cx, |s, cx| {
            s.select_space(Some(space_id.clone()), cx);
        });
        let target = {
            let state = self.state.read(cx);
            let in_space = |id: &str| {
                state
                    .visible_chats()
                    .any(|c| c.id == id && c.space_id.as_deref() == Some(space_id.as_str()))
            };
            self.space_last_chat
                .get(&space_id)
                .filter(|id| in_space(id))
                .cloned()
                .or_else(|| {
                    // `visible_chats` is recency-sorted — first match is the
                    // most recent chat of the space.
                    state
                        .visible_chats()
                        .find(|c| c.space_id.as_deref() == Some(space_id.as_str()))
                        .map(|c| c.id.clone())
                })
        };
        self.state.update(cx, |s, cx| s.select_chat(target, cx));
        // Activation discloses: what actually changed (this space's sessions
        // now fill the tab strip) is what the sidebar shows changing.
        self.expand_space(&space_id, cx);
        self.settings.last_space_id = Some(space_id);
        self.schedule_save(cx);
        cx.notify();
    }

    // ---- disclosure (gh#124) ----

    pub(super) fn space_expanded(&self, space_id: &str) -> bool {
        self.settings
            .expanded_spaces
            .iter()
            .any(|id| id == space_id)
    }

    /// Disclose a space's sessions (idempotent). Persisted device-local.
    pub(super) fn expand_space(&mut self, space_id: &str, cx: &mut Context<Self>) {
        if !self.space_expanded(space_id) {
            self.settings.expanded_spaces.push(space_id.to_string());
            self.schedule_save(cx);
            cx.notify();
        }
    }

    /// The chevron's toggle — collapse if disclosed, disclose if not. Does not
    /// activate the space; that is the row's job.
    fn toggle_space_disclosure(&mut self, space_id: &str, cx: &mut Context<Self>) {
        if let Some(ix) = self
            .settings
            .expanded_spaces
            .iter()
            .position(|id| id == space_id)
        {
            self.settings.expanded_spaces.remove(ix);
        } else {
            self.settings.expanded_spaces.push(space_id.to_string());
        }
        self.schedule_save(cx);
        cx.notify();
    }

    // ---- repo links (gh#124) ----

    /// Sweep the hosts for `space → owner/repo` links and fold them into
    /// standing state ([`AppState::apply_space_slugs`]) — the same
    /// [`methods::LIST_REPO_SPACES`] contract the ⌘K palette uses, run
    /// whenever the space/device membership changes so the sidebar can name
    /// spaces by their repo without opening the palette first.
    pub(super) fn refresh_space_slugs(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let (devices, local) = {
            let state = self.state.read(cx);
            (state.devices.clone(), state.local_device_id.clone())
        };
        let candidates = board::host_candidates(&devices, local.as_deref());
        self.slug_task = Some(cx.spawn(async move |this, cx| {
            let mut links: Vec<SpaceSlug> = Vec::new();
            for candidate in candidates {
                let mut params = serde_json::json!({});
                if let (Some(target), Some(object)) = (candidate.as_deref(), params.as_object_mut())
                {
                    object.insert("targetDeviceId".into(), serde_json::json!(target));
                }
                let Ok(value) = engine.client().call(methods::LIST_REPO_SPACES, params).await
                else {
                    continue;
                };
                let Ok(reply) = serde_json::from_value::<RepoSpacesReply>(value) else {
                    continue;
                };
                links.extend(reply.spaces);
            }
            this.update(cx, |shell, cx| {
                shell
                    .state
                    .update(cx, |s, _| s.apply_space_slugs(links));
                cx.notify();
            })
            .ok();
        }));
    }

    // ---- sidebar sections ----

    /// The "Spaces" section: tracked header + add button, then the spaces
    /// grouped by device — the device named ONCE per group — with each space's
    /// sessions disclosed inline under its row (gh#124).
    ///
    /// `active` is [`spaces_view::active_placements`] for the whole sidebar:
    /// which chats the Active list above is drawing, and where each lives. The
    /// tree answers "what lives here" WITHOUT re-listing what Active already
    /// says is alive (gh#138) — a space row carries the count instead, and a
    /// space whose sessions are all up there says so where its rows would be.
    pub(super) fn render_spaces_section(
        &mut self,
        active: &[(String, Option<String>)],
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // A drag that ended off-list (no drop event) must not strand the
        // sibling slide offsets.
        if self.space_drag.is_some() && !cx.has_active_drag() {
            self.space_drag = None;
        }
        let borrowed_active: Vec<(&str, Option<&str>)> = active
            .iter()
            .map(|(chat, space)| (chat.as_str(), space.as_deref()))
            .collect();
        let (spaces, selected, device_names, device_presence, attention, slugs, local_device) = {
            let now = Utc::now();
            let state = self.state.read(cx);
            let spaces = state.spaces.clone();
            let device_names: std::collections::HashMap<String, String> = spaces
                .iter()
                .map(|s| {
                    (
                        s.device_id.clone(),
                        state
                            .device_name(&s.device_id)
                            .unwrap_or("Unknown device")
                            .to_string(),
                    )
                })
                .collect();
            // Host-presence (the revived "Remote" signal), three-state (gh#126):
            // a lapsed heartbeat reads "offline" (a host outage, not slow sync)
            // only while THIS app's engine can hear — with our own sync rooms
            // down the row indicts the pipe instead of the box.
            let device_presence: std::collections::HashMap<String, crate::state::HostPresence> =
                spaces
                    .iter()
                    .map(|s| {
                        (
                            s.device_id.clone(),
                            state.host_presence(&s.device_id, now),
                        )
                    })
                    .collect();
            // Spaces with a live/awaiting session get an aggregate dot (the
            // most urgent member status wins) so the attention signal survives
            // a collapsed row.
            let mut attention: std::collections::HashMap<String, ChatIndicator> =
                std::collections::HashMap::new();
            for chat in state.visible_chats() {
                let status = state.display_status_for(chat, now);
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
                        if crate::state::attention_rank(status)
                            < crate::state::attention_rank(*held)
                        {
                            *held = status;
                        }
                    })
                    .or_insert(status);
            }
            (
                spaces,
                state.selected_space.clone(),
                device_names,
                device_presence,
                attention,
                state.space_slugs.clone(),
                state.local_device_id.clone(),
            )
        };
        // Manual (drag) order overrides the synced creation order — device-
        // local, resolved exactly like the session-tab order.
        let spaces: Vec<Space> = {
            let created: Vec<String> = spaces.iter().map(|s| s.id.clone()).collect();
            let order = super::tabs::resolve_tab_order(&created, &self.settings.space_order);
            let mut by_id: std::collections::HashMap<String, Space> =
                spaces.into_iter().map(|s| (s.id.clone(), s)).collect();
            order.iter().filter_map(|id| by_id.remove(id)).collect()
        };

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px(px(Theme::SPACE_SM))
            .pt(px(8.0))
            .pb(px(4.0))
            .child(
                div()
                    .text_size(px(11.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted.opacity(0.6))
                    .child(SharedString::from("Spaces")),
            )
            .child(
                div()
                    .id("add-space")
                    .size(px(20.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .bg(motion::hover_blend(
                        "add-space",
                        theme.wash(0.0),
                        theme.wash(0.14),
                    ))
                    .on_hover(motion::hover_listener("add-space"))
                    .on_click(cx.listener(|this, _, _, cx| this.open_add_space(cx)))
                    .child(
                        icon(icons::PLUS)
                            .size(px(14.0))
                            .text_color(theme.text_muted.opacity(0.7)),
                    ),
            );

        let mut column = div().flex().flex_col().child(header);
        if spaces.is_empty() {
            // Ghost row: the empty-state affordance mirrors a space row.
            column = column.child(
                div()
                    .id("add-space-ghost")
                    .mx(px(0.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::SPACE_SM))
                    .rounded(px(8.0))
                    .px(px(Theme::SPACE_SM))
                    .py(px(6.0))
                    .text_size(px(13.0))
                    .text_color(motion::hover_blend(
                        "add-space-ghost",
                        theme.text_muted,
                        theme.text,
                    ))
                    .bg(motion::hover_blend(
                        "add-space-ghost",
                        theme.wash(0.0),
                        theme.element_hover,
                    ))
                    .on_hover(motion::hover_listener("add-space-ghost"))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| this.open_add_space(cx)))
                    .child(
                        icon(icons::FOLDER)
                            .size(px(16.0))
                            .text_color(theme.text_muted),
                    )
                    .child(SharedString::from("Add a repo")),
            );
        } else {
            // While a drag is live every space renders collapsed, so a device
            // group's rows are uniform [`SPACE_ROW_SLOT`]s and the drop-index
            // math holds.
            let drag_active = self.space_drag.is_some();
            let groups: Vec<(String, Vec<Space>)> =
                spaces_view::device_groups(&spaces, local_device.as_deref())
                    .into_iter()
                    .map(|g| {
                        (
                            g.device_id.to_string(),
                            g.spaces.into_iter().cloned().collect(),
                        )
                    })
                    .collect();
            for (device_id, group) in groups {
                // The device's name appears ONCE, over the spaces it hosts —
                // and only for a REMOTE host: it is an address, and your own
                // machine needs none. Offline rides here too, said once.
                if local_device.as_deref() != Some(device_id.as_str()) {
                    let device_name = device_names
                        .get(&device_id)
                        .cloned()
                        .unwrap_or_else(|| "Unknown device".to_string());
                    let presence = device_presence
                        .get(&device_id)
                        .copied()
                        .unwrap_or(crate::state::HostPresence::Online);
                    column = column.child(Self::render_device_header(
                        device_name,
                        presence,
                        theme,
                    ));
                }
                let count = group.len();
                let drag = self
                    .space_drag
                    .as_ref()
                    .filter(|d| d.device == device_id)
                    .map(|d| (d.from, d.over, d.epoch, d.prev_over));
                // Repo-first names, made unique inside this group (gh#138): a
                // repo has one slug and any number of checkouts on a machine,
                // and two rows reading `owner/repo` look like one row drawn
                // twice.
                let titles = spaces_view::space_titles(
                    &group
                        .iter()
                        .map(|space| (space, slugs.get(&space.id).map(String::as_str)))
                        .collect::<Vec<_>>(),
                );
                let mut rows: Vec<AnyElement> = Vec::new();
                for (ix, space) in group.into_iter().enumerate() {
                    let id = space.id.clone();
                    let is_selected = selected.as_deref() == Some(space.id.as_str());
                    let space_attention = attention.get(&space.id).copied();
                    let slug = slugs.get(&space.id).map(String::as_str);
                    let title: SharedString = titles[ix].clone().into();
                    let repo = slug.is_some();
                    let expanded = self.space_expanded(&space.id) && !drag_active;
                    // What Active holds for this space — the count the row
                    // wears, and the rows the shelf therefore skips.
                    let order = self.tab_ids(&id, cx);
                    let shelf = spaces_view::space_shelf(
                        &id,
                        order.iter().map(String::as_str),
                        &borrowed_active,
                    );
                    let running = shelf.running;
                    let row = self.render_space_row(
                        ix,
                        device_id.clone(),
                        space,
                        title,
                        repo,
                        expanded,
                        is_selected,
                        space_attention,
                        running,
                        theme,
                        cx,
                    );
                    // Sliding transform while a sibling is dragged over —
                    // the session-tab idiom, vertical.
                    rows.push(match drag {
                        Some((from, over, epoch, prev_over)) if ix != from => {
                            let target = slide_offset(ix, from, over) * SPACE_ROW_SLOT;
                            let start = slide_offset(ix, from, prev_over) * SPACE_ROW_SLOT;
                            div()
                                .relative()
                                .child(row.with_animation(
                                    SharedString::from(format!("space-slide-{id}-{epoch}")),
                                    TAB_SLIDE.animation(),
                                    move |el, t| el.top(px(motion::lerp(start, target, t))),
                                ))
                                .into_any_element()
                        }
                        // The dragged row renders as an invisible spacer; the
                        // cursor ghost represents it.
                        Some((from, ..)) if ix == from => div()
                            .h(px(SPACE_ROW_SLOT - 2.0))
                            .flex_none()
                            .into_any_element(),
                        _ => row.into_any_element(),
                    });
                    // The disclosure: the space's IDLE sessions, inline under
                    // its row (gh#124's containment, gh#138's division of
                    // labour — the live ones have their row in Active).
                    if expanded {
                        rows.push(self.render_space_sessions(&shelf, theme, cx));
                    }
                }
                let group_device = device_id.clone();
                column = column.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .on_drag_move::<SpaceDragPayload>(cx.listener({
                            let device = device_id.clone();
                            move |this, event: &gpui::DragMoveEvent<SpaceDragPayload>, _, cx| {
                                let payload = event.drag(cx);
                                // A drag only lives inside its own device
                                // group — a space cannot switch devices.
                                if payload.device != device {
                                    return;
                                }
                                let from = payload.from;
                                let rel_y = f32::from(event.event.position.y)
                                    - f32::from(event.bounds.top());
                                let over = drop_index(rel_y, SPACE_ROW_SLOT, count);
                                this.update_space_drag_over(device.clone(), from, over, cx);
                            }
                        }))
                        .on_drop::<SpaceDragPayload>(cx.listener(
                            move |this, payload: &SpaceDragPayload, _, cx| {
                                if payload.device != group_device {
                                    return;
                                }
                                let to = this
                                    .space_drag
                                    .as_ref()
                                    .map(|d| d.over)
                                    .unwrap_or(payload.from);
                                this.commit_space_reorder(
                                    group_device.clone(),
                                    payload.from,
                                    to,
                                    cx,
                                );
                            },
                        ))
                        .children(rows),
                );
            }
        }
        column.into_any_element()
    }

    /// A device group's one-line header: "@ name", with "· offline" appended
    /// in the warning tone when the host's presence heartbeat lapsed. This is
    /// the ONLY place the sidebar names a device (gh#124) — it used to ride on
    /// every space row, the sidebar's loudest element spent on its least
    /// differentiating fact.
    fn render_device_header(
        name: String,
        presence: crate::state::HostPresence,
        theme: &Theme,
    ) -> AnyElement {
        use crate::state::HostPresence;
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .px(px(Theme::SPACE_SM))
            .pt(px(6.0))
            .pb(px(2.0))
            .text_size(px(11.0))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(theme.text_muted.opacity(0.5))
                    .child(SharedString::from(format!("@ {name}"))),
            )
            // Three-state (gh#126), said ONCE per group: amber "offline" only
            // when this app's engine can hear and the host's beat lapsed;
            // muted "sync down" when our own pipe is the broken thing.
            .when(matches!(presence, HostPresence::Offline), |el| {
                el.child(
                    div()
                        .flex_none()
                        .text_color(theme.warning.opacity(0.8))
                        .child(SharedString::from("· offline")),
                )
            })
            .when(matches!(presence, HostPresence::SyncDown), |el| {
                el.child(
                    div()
                        .flex_none()
                        .text_color(theme.text_muted.opacity(0.5))
                        .child(SharedString::from("· sync down")),
                )
            })
            .into_any_element()
    }

    /// Track the drop slot while a space row is dragged over its device group
    /// (150ms sibling slides restart per committed `over` change).
    fn update_space_drag_over(
        &mut self,
        device: String,
        from: usize,
        over: usize,
        cx: &mut Context<Self>,
    ) {
        match &mut self.space_drag {
            Some(drag) if drag.device == device && drag.from == from => {
                if drag.over != over {
                    drag.prev_over = drag.over;
                    drag.over = over;
                    drag.epoch += 1;
                    cx.notify();
                }
            }
            _ => {
                self.space_drag = Some(SpaceDragState {
                    device,
                    from,
                    over,
                    epoch: 0,
                    prev_over: from,
                });
                cx.notify();
            }
        }
    }

    /// Commit a drag: persist the new visual order. The indices are local to
    /// the device group ([`SpaceDragState`]); the persisted order is the
    /// groups' orders concatenated in display order, which round-trips through
    /// [`super::tabs::resolve_tab_order`] + [`spaces_view::device_groups`].
    fn commit_space_reorder(
        &mut self,
        device: String,
        from: usize,
        to: usize,
        cx: &mut Context<Self>,
    ) {
        let (spaces, local_device) = {
            let state = self.state.read(cx);
            (state.spaces.clone(), state.local_device_id.clone())
        };
        // Same resolution the render used, so the indices agree with what was
        // on screen.
        let ordered: Vec<Space> = {
            let created: Vec<String> = spaces.iter().map(|s| s.id.clone()).collect();
            let order = super::tabs::resolve_tab_order(&created, &self.settings.space_order);
            let mut by_id: std::collections::HashMap<String, Space> =
                spaces.into_iter().map(|s| (s.id.clone(), s)).collect();
            order.iter().filter_map(|id| by_id.remove(id)).collect()
        };
        let mut new_order: Vec<String> = Vec::new();
        for group in spaces_view::device_groups(&ordered, local_device.as_deref()) {
            let mut ids: Vec<String> = group.spaces.iter().map(|s| s.id.clone()).collect();
            if group.device_id == device && from < ids.len() {
                reorder_tabs(&mut ids, from, to);
            }
            new_order.extend(ids);
        }
        self.settings.space_order = new_order;
        self.schedule_save(cx);
        self.space_drag = None;
        cx.notify();
    }

    /// One space row: repo glyph + repo-first title, trailing disclosure
    /// chevron. No device suffix — the device is named once per group
    /// ([`Self::render_device_header`]). Click activates (and discloses);
    /// the chevron toggles disclosure without activating.
    ///
    /// `running` is how many of the space's chats the Active list is drawing
    /// (gh#138). It reads as a compact `· 3 running` beside the title — the
    /// fact that keeps the two surfaces tied together now that the shelf below
    /// no longer repeats those rows.
    #[allow(clippy::too_many_arguments)]
    fn render_space_row(
        &self,
        ix: usize,
        device: String,
        space: Space,
        title: SharedString,
        repo: bool,
        expanded: bool,
        selected: bool,
        attention: Option<ChatIndicator>,
        running: usize,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let id = space.id.clone();
        let fade_key = format!("space-row-{id}");
        let rest_bg = if selected {
            theme.glass_selected_bg()
        } else {
            theme.wash(0.0)
        };
        let rest_text = if selected {
            theme.text
        } else {
            theme.text.opacity(0.8)
        };
        let select_id = id.clone();
        let menu_id = id.clone();
        let chevron_id = id.clone();
        let chevron_key = format!("space-disclose-{id}");
        div()
            .id(SharedString::from(format!("space-{id}")))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .py(px(6.0))
            .text_color(motion::hover_blend(&fade_key, rest_text, theme.text))
            .bg(motion::hover_blend(&fade_key, rest_bg, theme.element_hover))
            .when(selected, |el| el.shadow(theme.glass_selected_shadows()))
            .on_hover(motion::hover_listener(fade_key))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.activate_space(select_id.clone(), cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.space_menu = Some((menu_id.clone(), event.position));
                    cx.notify();
                }),
            )
            .on_drag(
                SpaceDragPayload {
                    device,
                    from: ix,
                    name: title.clone(),
                    repo,
                },
                |payload, _point, _, cx| {
                    let name = payload.name.clone();
                    let repo = payload.repo;
                    cx.stop_propagation();
                    cx.new(|_| SpaceGhost { name, repo })
                },
            )
            // Status dot LEADS the row (like session rows) so its position is
            // stable — appearing/disappearing at the right edge made the row
            // jitter (user request). Faint at rest, colored under attention.
            .child(
                div()
                    .size(px(6.0))
                    .rounded_full()
                    .flex_none()
                    .bg(attention
                        .map(|status| status_dot_color(status, theme))
                        .unwrap_or_else(|| theme.white_alpha(0.14))),
            )
            // The glyph says what a space IS in a repo-first product: a repo
            // (a host supplied the gh#118 link), or a plain folder for the
            // scratch directory that is nobody's repo. Not the OS symbol for
            // "opens" — this row is a container, and the chevron carries that.
            .child(
                icon(if repo { icons::GITHUB_MARK } else { icons::FOLDER })
                    .size(px(16.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(13.0))
                    .line_height(px(17.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(title),
            )
            // "· 3 running" (gh#138): where the space's live rows went. Muted
            // and after the name — the dot already said how urgent, this says
            // how many, and neither needs to shout.
            .when_some(spaces_view::running_label(running), |el, label| {
                el.child(
                    div()
                        .flex_none()
                        .text_size(px(11.0))
                        .line_height(px(17.0))
                        .font_weight(gpui::FontWeight::NORMAL)
                        .text_color(theme.text_muted.opacity(0.6))
                        .child(SharedString::from(label)),
                )
            })
            .child(div().flex_1())
            // The disclosure chevron: glyph-swapped like the Changes-pane fold
            // (gpui at this rev has no rotation transform). Click toggles
            // without activating the space.
            .child(
                div()
                    .id(SharedString::from(chevron_key.clone()))
                    .size(px(18.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .bg(motion::hover_blend(
                        &chevron_key,
                        theme.wash(0.0),
                        theme.wash(0.14),
                    ))
                    .on_hover(motion::hover_listener(chevron_key))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_space_disclosure(&chevron_id, cx);
                    }))
                    .child(
                        icon(if expanded {
                            icons::ALT_ARROW_DOWN
                        } else {
                            icons::ALT_ARROW_RIGHT
                        })
                        .size(px(12.0))
                        .text_color(theme.text_muted.opacity(0.7)),
                    ),
            )
    }

    /// A space's disclosed sessions: the tab strip's order (creation + manual
    /// drag), vertical, inset under an indent rule so the containment is
    /// visible. No space name repeated on the row, because the row is IN the
    /// space.
    ///
    /// The shelf draws what Active is NOT drawing (gh#138). When Active holds
    /// every one of them it says so ([`spaces_view::shelf_note`]) rather than
    /// disclosing into a gap — expanding a space must always answer with
    /// something true about what lives here.
    fn render_space_sessions(
        &self,
        shelf: &spaces_view::SpaceShelf<'_>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let now = Utc::now();
        let order = &shelf.idle;
        let rows: Vec<AnyElement> = {
            let (selected, pinned) = {
                let state = self.state.read(cx);
                (state.selected_chat.clone(), state.orchestrator.clone())
            };
            order
                .iter()
                .filter_map(|chat_id| {
                    let (chat, status) = {
                        let state = self.state.read(cx);
                        let chat = state.visible_chats().find(|c| c.id == *chat_id)?.clone();
                        let status = state.display_status_for(&chat, now);
                        (chat, status)
                    };
                    Some(self.render_space_session_row(
                        &chat,
                        status,
                        selected.as_deref() == Some(*chat_id),
                        pinned.as_deref() == Some(*chat_id),
                        now,
                        theme,
                        cx,
                    ))
                })
                .collect()
        };
        let body: AnyElement = if rows.is_empty() {
            // Either the space has no sessions, or Active is holding all of
            // them — and the difference is exactly what a reader who just
            // expanded an obviously-busy space needs told.
            let note = spaces_view::shelf_note(shelf)
                .unwrap_or_else(|| "No sessions yet".to_string());
            div()
                .px(px(Theme::SPACE_SM))
                .py(px(4.0))
                .text_size(px(12.0))
                .text_color(theme.text_faint)
                .child(SharedString::from(note))
                .into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .children(rows)
                .into_any_element()
        };
        // The indent rule starts under the space row's status dot and hands
        // the rows a visible left edge: these BELONG to the row above.
        div()
            .ml(px(11.0))
            .pl(px(6.0))
            .my(px(2.0))
            .border_l_1()
            .border_color(theme.white_alpha(0.08))
            .flex()
            .flex_col()
            .child(body)
            .into_any_element()
    }

    /// One disclosed session: status rail + title + time-ago, harness/branch
    /// underneath. Two lines, not three — the space line is gone because the
    /// nesting already says it (gh#124's "no information twice").
    #[allow(clippy::too_many_arguments)]
    fn render_space_session_row(
        &self,
        chat: &comet_proto::Chat,
        status: ChatIndicator,
        selected: bool,
        orchestrator: bool,
        now: DateTime<Utc>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = chat.id.clone();
        let title: SharedString = transcript::single_line(
            &chat.title.clone().unwrap_or_else(|| "New session".into()),
        )
        .into();
        let time_ago: SharedString =
            format_time_ago(chat.last_message_at.unwrap_or(chat.created_at), now).into();
        let harness = chat.config.as_ref().map(|c| c.harness);
        let branch: Option<SharedString> = chat
            .branch
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .map(|b| SharedString::from(b.to_string()));
        let subline = theme.text_muted.opacity(0.5);
        let status_rail: AnyElement = if status == ChatIndicator::Working {
            div()
                .w(px(6.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .child(loaders::mini_gradient_spinner(
                    format!("chat-working-{id}"),
                    2.0,
                ))
                .into_any_element()
        } else {
            div()
                .size(px(6.0))
                .rounded_full()
                .flex_none()
                .bg(status_dot_color(status, theme))
                .into_any_element()
        };
        let fade_key = format!("chat-row-{id}");
        let rest_bg = if selected {
            theme.glass_selected_bg()
        } else {
            theme.wash(0.0)
        };
        let rest_text = if selected {
            theme.text
        } else {
            theme.text.opacity(0.8)
        };
        let select_id = id.clone();
        let menu_id = id.clone();
        div()
            .id(SharedString::from(format!("chat-{id}")))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .py(px(6.0))
            .text_color(motion::hover_blend(&fade_key, rest_text, theme.text))
            .bg(motion::hover_blend(&fade_key, rest_bg, theme.element_hover))
            .when(selected, |el| el.shadow(theme.glass_selected_shadows()))
            .on_hover(motion::hover_listener(fade_key))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                let id = select_id.clone();
                this.state.update(cx, |s, cx| s.select_chat(Some(id), cx));
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.chat_menu = Some((menu_id.clone(), event.position));
                    cx.notify();
                }),
            )
            // Line 1: status rail, ◆ for the orchestrator's chat, title, time.
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::SPACE_SM))
                    .child(status_rail)
                    .when(orchestrator, |el| {
                        el.child(
                            div()
                                .flex_none()
                                .text_size(px(10.0))
                                .line_height(px(14.0))
                                .text_color(theme.accent)
                                .child(SharedString::from(
                                    comet_proto::view::board::ORCHESTRATOR_GLYPH,
                                )),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(13.0))
                            .line_height(px(17.0))
                            .child(title),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.0))
                            .text_color(subline)
                            .child(time_ago),
                    ),
            )
            // Line 2: harness brand mark; sessions with a branch append it.
            .child(
                div()
                    .w_full()
                    .pl(px(14.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .when_some(
                        harness.map(crate::pickers::harness_brand_icon),
                        |el, (path, tint)| {
                            el.child(
                                icon(path)
                                    .size(px(11.0))
                                    .flex_none()
                                    .text_color(tint.unwrap_or(subline).opacity(0.8)),
                            )
                        },
                    )
                    .when_some(branch, |el, branch| {
                        el.child(
                            icon(icons::GIT_BRANCH)
                                .size(px(11.0))
                                .flex_none()
                                .text_color(subline),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(11.0))
                                .line_height(px(14.0))
                                .text_color(subline)
                                .child(branch),
                        )
                    }),
            )
            .into_any_element()
    }

    /// The "Needs you" inbox (gh#122): the first section, and the one that
    /// cannot miss. Every row says WHO and WHAT in words — no dot vocabulary —
    /// and when nothing is pending the section says so ([`needs_view::ALL_CLEAR`])
    /// instead of leaving a gap: the empty state is the reward, and it is what
    /// licenses everything below to stay calm.
    pub(super) fn render_needs_section(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let now = Utc::now();
        let needs = self.board.read(cx).needs(cx, now);
        let selected = self.state.read(cx).selected_chat.clone();

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px(px(Theme::SPACE_SM))
            .pt(px(12.0))
            .pb(px(4.0))
            .child(
                div()
                    .text_size(px(11.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted.opacity(0.6))
                    .child(SharedString::from(needs_view::NEEDS_YOU_TITLE)),
            )
            // The count is the header's whole answer: how many things want me.
            .when(!needs.is_empty(), |el| {
                el.child(
                    div()
                        .px(px(5.0))
                        .py(px(1.0))
                        .rounded(px(5.0))
                        .bg(theme.accent.opacity(0.16))
                        .text_size(px(10.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.accent)
                        .child(SharedString::from(format!("{}", needs.len()))),
                )
            });

        let body: AnyElement = if needs.is_empty() {
            // The quiet check, in words.
            div()
                .px(px(Theme::SPACE_SM))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .flex_none()
                        .text_size(px(10.0))
                        .text_color(status_dot_color(ChatIndicator::Completed, theme))
                        .child(SharedString::from("✓")),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from(needs_view::ALL_CLEAR)),
                )
                .into_any_element()
        } else {
            let rows: Vec<AnyElement> = needs
                .iter()
                .map(|need| {
                    let is_selected = selected.as_deref() == Some(need.chat_id.as_str());
                    self.render_need_row(need, is_selected, theme, cx)
                })
                .collect();
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .children(rows)
                .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .child(header)
            .child(body)
            .into_any_element()
    }

    /// One thing waiting on a human: kind glyph, WHO, the one-line WHAT under
    /// it. Click opens the chat, which is where answering happens.
    fn render_need_row(
        &self,
        need: &needs_view::NeedRow,
        selected: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use needs_view::NeedKind;
        let accent = match need.kind {
            NeedKind::Question => theme.accent,
            NeedKind::DeadRun => theme.danger,
            NeedKind::Report => status_dot_color(ChatIndicator::Completed, theme),
        };
        let subline = theme.text_muted.opacity(0.6);
        let fade_key = format!("need-row-{}", need.chat_id);
        let rest_bg = if selected {
            theme.glass_selected_bg()
        } else {
            theme.wash(0.0)
        };
        let rest_text = if selected {
            theme.text
        } else {
            theme.text.opacity(0.8)
        };
        let chat_id = need.chat_id.clone();

        div()
            .id(SharedString::from(format!("need-{}", need.chat_id)))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .py(px(6.0))
            .text_color(motion::hover_blend(&fade_key, rest_text, theme.text))
            .bg(motion::hover_blend(&fade_key, rest_bg, theme.element_hover))
            .when(selected, |el| el.shadow(theme.glass_selected_shadows()))
            .on_hover(motion::hover_listener(fade_key))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                let id = chat_id.clone();
                this.state.update(cx, |s, cx| s.select_chat(Some(id), cx));
            }))
            // Line 1: the kind's glyph, then WHO.
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .w(px(8.0))
                            .flex_none()
                            .text_size(px(9.0))
                            .text_color(accent)
                            .child(SharedString::from(need.kind.glyph())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(13.0))
                            .line_height(px(17.0))
                            .child(SharedString::from(transcript::single_line(&need.who))),
                    ),
            )
            // Line 2: WHAT, aligned under WHO.
            .child(
                div()
                    .w_full()
                    .pl(px(14.0))
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.0))
                    .line_height(px(14.0))
                    .text_color(subline)
                    .child(SharedString::from(need.what.clone())),
            )
            .into_any_element()
    }

    /// The orchestrator's fixed slot above Spaces (gh#122): a pinned thread —
    /// ◆ identity, the name, an unread badge, the latest report's preview —
    /// not a decorated session row. `None` only when no orchestrator is pinned
    /// (or its chat has not synced here); a pinned-but-silent orchestrator
    /// renders as the empty fixture, teaching where to look before the first
    /// notice arrives.
    pub(super) fn render_orchestrator_slot(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let now = Utc::now();
        let slot = self.state.read(cx).orchestrator_slot(now)?;
        let selected = self.state.read(cx).selected_chat.as_deref() == Some(slot.chat_id.as_str());

        let subline = theme.text_muted.opacity(0.6);
        let fade_key = format!("orch-slot-{}", slot.chat_id);
        let rest_bg = if selected {
            theme.glass_selected_bg()
        } else {
            theme.wash(0.0)
        };
        let rest_text = if selected {
            theme.text
        } else {
            theme.text.opacity(0.8)
        };
        let chat_id = slot.chat_id.clone();
        // The ◆ is identity; state is carried honestly beside it — the spinner
        // while a turn runs, so an 8h-old report can never be mistaken for a
        // turn running now.
        let lead: AnyElement = if slot.indicator == ChatIndicator::Working {
            div()
                .w(px(8.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .child(loaders::mini_gradient_spinner(
                    format!("orch-working-{}", slot.chat_id),
                    2.0,
                ))
                .into_any_element()
        } else {
            div()
                .w(px(8.0))
                .flex_none()
                .text_size(px(9.0))
                .text_color(theme.accent)
                .child(SharedString::from(
                    comet_proto::view::board::ORCHESTRATOR_GLYPH,
                ))
                .into_any_element()
        };
        // The right column: the badge while something is unread, the time it
        // last spoke otherwise. Words either way.
        let tail: AnyElement = if slot.unseen {
            div()
                .flex_none()
                .px(px(5.0))
                .py(px(1.0))
                .rounded(px(5.0))
                .bg(status_dot_color(ChatIndicator::Completed, theme).opacity(0.16))
                .text_size(px(10.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(status_dot_color(ChatIndicator::Completed, theme))
                .child(SharedString::from("new"))
                .into_any_element()
        } else {
            div()
                .flex_none()
                .text_size(px(11.0))
                .text_color(subline)
                .child(SharedString::from(
                    slot.last_at
                        .map(|at| format_time_ago(at, now))
                        .unwrap_or_default(),
                ))
                .into_any_element()
        };
        let preview = slot
            .preview
            .clone()
            .unwrap_or_else(|| needs_view::NO_REPORTS.to_string());
        // The latest report is the payload: brighter while unread.
        let preview_color = if slot.unseen {
            theme.text.opacity(0.7)
        } else {
            subline
        };

        Some(
            div()
                .id(SharedString::from(format!("orchestrator-{}", slot.chat_id)))
                .mt(px(12.0))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .rounded(px(8.0))
                .px(px(Theme::SPACE_SM))
                .py(px(6.0))
                .text_color(motion::hover_blend(&fade_key, rest_text, theme.text))
                .bg(motion::hover_blend(&fade_key, rest_bg, theme.element_hover))
                .when(selected, |el| el.shadow(theme.glass_selected_shadows()))
                .on_hover(motion::hover_listener(fade_key))
                .cursor_pointer()
                // Opening it opens the thread — and marks it seen, the synced
                // marker that clears the badge on every device.
                .on_click(cx.listener(move |this, _, _, cx| {
                    let id = chat_id.clone();
                    this.state.update(cx, |s, cx| s.select_chat(Some(id), cx));
                }))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(lead)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(13.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .line_height(px(17.0))
                                .child(SharedString::from(needs_view::ORCHESTRATOR_NAME)),
                        )
                        .child(tail),
                )
                .child(
                    div()
                        .w_full()
                        .pl(px(14.0))
                        .min_w_0()
                        .truncate()
                        .text_size(px(11.0))
                        .line_height(px(14.0))
                        .text_color(preview_color)
                        .child(SharedString::from(preview)),
                )
                .into_any_element(),
        )
    }

    /// The "Active" section (gh#123): everything alive on the box in one
    /// group — live board attempts (gh#103) and the working chats the board
    /// never released (gh#117), needs-you first, then working, blind to how
    /// each one started. Origin still shows on the row: an attempt wears its
    /// issue identifier as a chip and keeps its branch and cap, an unmanaged
    /// run is its own bare title.
    ///
    /// `None` when nothing is running — an empty section here would be a
    /// permanent reminder that a board exists, on a machine that may host none.
    /// The rows come from the board panel's standing `WatchBoard` stream joined
    /// to chats and sessions ([`BoardPanel::active`]); the board dock stays the
    /// deep view, and this is the glance.
    ///
    /// Since gh#138 this list OWNS its chats: while a session is live its full
    /// row is here and nowhere else, and the space's shelf below picks it back
    /// up when it goes idle. So `active` arrives from the caller rather than
    /// being derived here — the spaces tree is derived from the same value on
    /// the same frame ([`Shell::render_chat_sidebar`]).
    pub(super) fn render_active_section(
        &mut self,
        active: Vec<comet_proto::view::board::ActiveRow>,
        now: DateTime<Utc>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        use comet_proto::view::board::ActiveRow;
        if active.is_empty() {
            return None;
        }
        let blocked = comet_proto::view::board::active_needing_attention(&active);
        let selected = self.state.read(cx).selected_chat.clone();

        let header = Self::render_active_header(blocked, theme);

        let rows: Vec<AnyElement> = active
            .into_iter()
            .map(|row| {
                let is_selected = selected.as_deref() == Some(row.chat_id());
                match row {
                    ActiveRow::Agent(agent) => {
                        self.render_agent_row(&agent, is_selected, now, theme, cx)
                    }
                    ActiveRow::Unmanaged(run) => {
                        self.render_running_row(&run, is_selected, now, theme, cx)
                    }
                }
            })
            .collect();

        Some(
            div()
                .flex()
                .flex_col()
                .child(header)
                .child(div().flex().flex_col().gap(px(2.0)).children(rows))
                .into_any_element(),
        )
    }

    /// The Active header: one label over the whole live list, and the count of
    /// rows under it that want a human.
    fn render_active_header(blocked: usize, theme: &Theme) -> AnyElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px(px(Theme::SPACE_SM))
            .pt(px(12.0))
            .pb(px(4.0))
            .child(
                div()
                    .text_size(px(11.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted.opacity(0.6))
                    .child(SharedString::from("Active")),
            )
            // The count is what you look for first: three running, one of them
            // stuck on a question you have not answered.
            .when(blocked > 0, |el| {
                el.child(
                    div()
                        .px(px(5.0))
                        .py(px(1.0))
                        .rounded(px(5.0))
                        .bg(theme.danger.opacity(0.16))
                        .text_size(px(10.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.danger)
                        .child(SharedString::from(format!("{blocked} blocked"))),
                )
            })
            .into_any_element()
    }

    /// The leading rail both live-work rows carry: a spinner while working (the
    /// session-row idiom, and the one state where motion says something no
    /// glyph can), the board's own glyph otherwise — so a row means the same
    /// thing here as it does one keystroke away in the board pane.
    fn render_agent_rail(
        key: &str,
        state: comet_proto::view::board::AgentState,
        accent: gpui::Hsla,
    ) -> AnyElement {
        use comet_proto::view::board::AgentState;
        if state == AgentState::Working {
            div()
                .w(px(8.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .child(loaders::mini_gradient_spinner(key.to_string(), 2.0))
                .into_any_element()
        } else {
            div()
                .w(px(8.0))
                .flex_none()
                .text_size(px(9.0))
                .text_color(accent)
                .child(SharedString::from(state.glyph()))
                .into_any_element()
        }
    }

    /// One unmanaged run (gh#117): state rail, the chat's own title, elapsed
    /// since the run started. One line, not two — there is no branch promised
    /// and no issue behind it, and a second line of nothing would make an agent
    /// row's second line look like it means less than it does. The bare title
    /// is also the origin telling: an attempt wears an identifier chip, and
    /// this row deliberately does not.
    fn render_running_row(
        &self,
        row: &comet_proto::view::board::RunningRow,
        selected: bool,
        now: DateTime<Utc>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let accent = crate::board::agent_state_color(row.state, theme);
        let rail = Self::render_agent_rail(
            &format!("running-working-{}", row.chat_id),
            row.state,
            accent,
        );
        let subline = theme.text_muted.opacity(0.6);
        let fade_key = format!("running-row-{}", row.chat_id);
        let rest_bg = if selected {
            theme.glass_selected_bg()
        } else {
            theme.wash(0.0)
        };
        let rest_text = if selected {
            theme.text
        } else {
            theme.text.opacity(0.8)
        };
        let chat_id = row.chat_id.clone();

        div()
            .id(SharedString::from(format!("running-{}", row.chat_id)))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .py(px(6.0))
            .text_color(motion::hover_blend(&fade_key, rest_text, theme.text))
            .bg(motion::hover_blend(&fade_key, rest_bg, theme.element_hover))
            .when(selected, |el| el.shadow(theme.glass_selected_shadows()))
            .on_hover(motion::hover_listener(fade_key))
            .cursor_pointer()
            // Opening it opens the transcript, which is where answering it
            // happens. `select_chat` lands in the chat's own space on its own,
            // exactly as the agent row and the Sessions list rely on.
            .on_click(cx.listener(move |this, _, _, cx| {
                let id = chat_id.clone();
                this.state.update(cx, |s, cx| s.select_chat(Some(id), cx));
            }))
            .child(rail)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(13.0))
                    .line_height(px(17.0))
                    .child(SharedString::from(transcript::single_line(&row.title))),
            )
            // A blocked run says so in words: it has no issue identifier to
            // recognise it by, so the glyph alone is doing too much work.
            .when(row.state.needs_attention(), |el| {
                el.child(
                    div()
                        .flex_none()
                        .text_size(px(11.0))
                        .text_color(accent.opacity(0.9))
                        .child(SharedString::from(row.state.label())),
                )
            })
            .when_some(row.elapsed_label(now), |el, label| {
                el.child(
                    div()
                        .flex_none()
                        .text_size(px(11.0))
                        .text_color(subline)
                        .child(SharedString::from(label)),
                )
            })
            .into_any_element()
    }

    /// One live attempt: state glyph, the issue identifier, elapsed against the
    /// route's cap, and the branch underneath. Click opens the chat.
    fn render_agent_row(
        &self,
        agent: &comet_proto::view::board::AgentRow,
        selected: bool,
        now: DateTime<Utc>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let accent = crate::board::agent_state_color(agent.state, theme);
        let rail = Self::render_agent_rail(
            &format!("agent-working-{}", agent.chat_id),
            agent.state,
            accent,
        );

        let (hover, text) = (theme.element_hover, theme.text);
        let subline = theme.text_muted.opacity(0.6);
        let fade_key = format!("agent-row-{}", agent.chat_id);
        let rest_bg = if selected {
            theme.glass_selected_bg()
        } else {
            theme.wash(0.0)
        };
        let rest_text = if selected { text } else { text.opacity(0.8) };
        let chat_id = agent.chat_id.clone();
        // Past the cap the counter IS the warning: gh#70's clock will interrupt
        // this agent, and the number is the reason.
        let elapsed_color = if agent.over_cap(now) {
            theme.warning
        } else {
            subline
        };

        div()
            .id(SharedString::from(format!("agent-{}", agent.chat_id)))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .py(px(6.0))
            .text_color(motion::hover_blend(&fade_key, rest_text, text))
            .bg(motion::hover_blend(&fade_key, rest_bg, hover))
            .when(selected, |el| el.shadow(theme.glass_selected_shadows()))
            .on_hover(motion::hover_listener(fade_key))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                let id = chat_id.clone();
                this.state.update(cx, |s, cx| s.select_chat(Some(id), cx));
            }))
            // Line 1: state glyph, the issue identifier as a chip, elapsed /
            // cap. The chip is the origin telling (gh#123): in a mixed Active
            // list it is what says "the board released this" at a glance, and
            // its fill is the sidebar's wash language, not an accent tint —
            // the accent stays on the state rail.
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(rail)
                    .child(
                        div().flex_1().min_w_0().flex().flex_row().child(
                            div()
                                .max_w_full()
                                .truncate()
                                .px(px(5.0))
                                .rounded(px(5.0))
                                .bg(theme.wash(0.11))
                                .text_size(px(12.0))
                                .line_height(px(17.0))
                                .child(SharedString::from(agent.identifier.clone())),
                        ),
                    )
                    .when_some(agent.elapsed_label(now), |el, label| {
                        el.child(
                            div()
                                .flex_none()
                                .text_size(px(11.0))
                                .text_color(elapsed_color)
                                .child(SharedString::from(label)),
                        )
                    }),
            )
            // Line 2: the branch, aligned under the identifier. A row with no
            // branch says what it is doing instead of leaving the line blank.
            .child(
                div()
                    .w_full()
                    .pl(px(14.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .when_some(agent.branch.clone(), |el, branch| {
                        el.child(
                            icon(icons::GIT_BRANCH)
                                .size(px(11.0))
                                .flex_none()
                                .text_color(subline),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(11.0))
                                .line_height(px(14.0))
                                .text_color(subline)
                                .child(SharedString::from(branch)),
                        )
                    })
                    .when(agent.branch.is_none(), |el| {
                        el.child(
                            div()
                                .text_size(px(11.0))
                                .line_height(px(14.0))
                                .text_color(accent.opacity(0.9))
                                .child(SharedString::from(agent.state.label())),
                        )
                    }),
            )
            .into_any_element()
    }

    // ---- add-space flow (the ⌘K palette) ----

    pub(super) fn open_add_space(&mut self, cx: &mut Context<Self>) {
        let devices: Vec<Device> = self.state.read(cx).devices.clone();
        let local = self.state.read(cx).local_device_id.clone();
        // Land on this device's tab (else the first registered device).
        let device = devices
            .iter()
            .find(|d| local.as_deref() == Some(d.id.as_str()))
            .or_else(|| devices.first())
            .cloned();
        // "PaletteSearch" context: navigation keys stay unbound so ↑↓/←/→/⏎
        // bubble to the palette frame (`add_space_key`) instead of moving the
        // text caret — Enter and ⌘Enter are both handled there.
        let search =
            cx.new(|cx| ComposerInput::with_context("Search repos…", "PaletteSearch", cx));
        let search_events = cx.subscribe(&search, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Edited) {
                if let Some(flow) = this.add_space.as_mut() {
                    flow.active = 0;
                }
                cx.notify();
            }
        });
        self.add_space = Some(AddSpaceFlow {
            mode: AddSpaceMode::Repos,
            hosts: Vec::new(),
            target: 0,
            repos: Loadable::Idle,
            onboarding: None,
            device,
            search,
            browser: Loadable::Idle,
            browser_path: None,
            home: None,
            browser_repo: false,
            active: 0,
            submit_busy: false,
            error: None,
            focus: cx.focus_handle(),
            list_scroll: gpui::ScrollHandle::new(),
            focus_pending: true,
            load_task: None,
            submit_task: None,
            repo_task: None,
            _search_events: search_events,
        });
        self.load_repo_hosts(cx);
        cx.notify();
    }

    /// Switch doors. The folder browser is loaded lazily — most opens of this
    /// palette never reach it.
    fn add_space_set_mode(&mut self, mode: AddSpaceMode, cx: &mut Context<Self>) {
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        if flow.mode == mode {
            return;
        }
        flow.mode = mode;
        flow.active = 0;
        flow.error = None;
        flow.list_scroll.set_offset(gpui::Point::default());
        let search = flow.search.clone();
        let needs_browse = mode == AddSpaceMode::Folders && matches!(flow.browser, Loadable::Idle);
        let has_device = flow.device.is_some();
        search.update(cx, |input, cx| {
            input.set_text("", cx);
            input.set_placeholder(
                match mode {
                    AddSpaceMode::Repos => "Search repos…",
                    AddSpaceMode::Folders => "Search folders…",
                },
                cx,
            );
        });
        if needs_browse && has_device {
            self.load_space_folders(None, cx);
        }
        cx.notify();
    }

    /// Ask every device whether it hosts a board, and take the whole list of
    /// answers (gh#118).
    ///
    /// Sweeping past the first answer costs almost nothing and buys the one fact
    /// onboarding needs: a device hosting no board refuses
    /// [`methods::LIST_REPO_SPACES`] before it does any git or GitHub work — the
    /// same "said nothing at all" contract the board panel rules candidates out
    /// with — so only real hosts are slow, and only real hosts are counted.
    fn load_repo_hosts(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            if let Some(flow) = self.add_space.as_mut() {
                flow.repos = Loadable::Error("Engine not connected".into());
            }
            return;
        };
        let (devices, local) = {
            let state = self.state.read(cx);
            (state.devices.clone(), state.local_device_id.clone())
        };
        let candidates = board::host_candidates(&devices, local.as_deref());
        if let Some(flow) = self.add_space.as_mut() {
            flow.repos = Loadable::Loading;
            flow.hosts.clear();
            flow.target = 0;
        }
        self.repo_task_set(cx.spawn(async move |this, cx| {
            let mut hosts: Vec<RepoHost> = Vec::new();
            for candidate in candidates {
                let mut params = serde_json::json!({});
                if let (Some(target), Some(object)) = (candidate.as_deref(), params.as_object_mut())
                {
                    object.insert("targetDeviceId".into(), serde_json::json!(target));
                }
                let Ok(value) = engine
                    .client()
                    .call(methods::LIST_REPO_SPACES, params)
                    .await
                else {
                    continue;
                };
                let Ok(reply) = serde_json::from_value::<RepoSpacesReply>(value) else {
                    continue;
                };
                hosts.push(RepoHost {
                    target: candidate,
                    device_id: reply.device_id,
                    links: reply.spaces,
                    offers: reply
                        .repos
                        .into_iter()
                        .map(|c| RepoOffer {
                            slug: c.slug,
                            private: c.private,
                            archived: c.archived,
                            on_board: c.missing.is_none(),
                        })
                        .collect(),
                    note: reply.repos_note.map(SharedString::from),
                });
            }
            this.update(cx, |shell, cx| {
                if let Some(flow) = shell.add_space.as_mut() {
                    flow.hosts = hosts;
                    flow.repos = Loadable::Ready(());
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn repo_task_set(&mut self, task: Task<()>) {
        if let Some(flow) = self.add_space.as_mut() {
            flow.repo_task = Some(task);
        }
    }

    /// Devices-rail click: rebrowse the same palette on another device.
    fn add_space_pick_device(&mut self, device: Device, cx: &mut Context<Self>) {
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        if flow.device.as_ref().is_some_and(|d| d.id == device.id) {
            return;
        }
        flow.device = Some(device);
        flow.browser = Loadable::Idle;
        flow.browser_path = None;
        flow.home = None;
        flow.browser_repo = false;
        flow.active = 0;
        flow.error = None;
        let search = flow.search.clone();
        search.update(cx, |input, cx| input.set_text("", cx));
        self.load_space_folders(None, cx);
        cx.notify();
    }

    // ---- the repo list (the front door, gh#118) ----

    /// The palette's repo rows, filtered by the search query.
    ///
    /// Every space is in here, whether or not the sweep found a board: a picker
    /// that showed nothing until a box answered would be useless on a laptop
    /// that has never had one, and the spaces are the part it already knows.
    fn add_space_repo_rows(&self, cx: &App) -> Vec<RepoRow> {
        let Some(flow) = self.add_space.as_ref() else {
            return Vec::new();
        };
        let spaces = self.state.read(cx).spaces.clone();
        let host = flow.host().map(|h| h.device_id.clone());
        let rows = repos::repo_rows(&spaces, &flow.links(), &flow.offers(), host.as_deref());
        repos::filter_rows(flow.search.read(cx).text(), &rows)
    }

    /// Act on the highlighted repo row: open its space, or clone it onto the box.
    fn add_space_activate_repo(&mut self, cx: &mut Context<Self>) {
        let rows = self.add_space_repo_rows(cx);
        let Some(flow) = self.add_space.as_ref() else {
            return;
        };
        let Some(row) = rows.get(flow.active).cloned() else {
            return;
        };
        self.add_space_pick_repo(row, cx);
    }

    /// One repo row picked (keyboard or mouse).
    ///
    /// A connected repo lands in its space with no further questions — the space
    /// knows its device, so there is nothing to ask and nothing to choose. An
    /// unconnected one runs the gh#97 onboard against the board's host, which is
    /// the only device that can do it.
    fn add_space_pick_repo(&mut self, row: RepoRow, cx: &mut Context<Self>) {
        if let Some(space_id) = row.space_id {
            self.add_space = None;
            self.activate_space(space_id, cx);
            return;
        }
        let Some(slug) = row.slug else {
            return;
        };
        self.submit_onboard(slug, cx);
    }

    /// Clone + createSpace + adopt on the board's host, then land in the result.
    ///
    /// Unary and slow — it is a `git clone` on another machine — so the palette
    /// stays open and busy around it rather than closing on a hope. Its refusals
    /// are written to be read ("the board's GitHub App cannot see it…") and land
    /// on the palette's own error line, not in Settings.
    fn submit_onboard(&mut self, slug: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            if let Some(flow) = self.add_space.as_mut() {
                flow.error = Some("Engine not connected".into());
            }
            cx.notify();
            return;
        };
        let Some(flow) = self.add_space.as_ref() else {
            return;
        };
        if flow.onboarding.is_some() {
            return;
        }
        let Some(host) = flow.host() else {
            if let Some(flow) = self.add_space.as_mut() {
                flow.error = Some(
                    "No device here hosts a board, so there is nowhere to clone this repo".into(),
                );
            }
            cx.notify();
            return;
        };
        let mut params = serde_json::json!({ "slug": slug });
        if let (Some(target), Some(object)) = (host.target.as_deref(), params.as_object_mut()) {
            object.insert("targetDeviceId".into(), serde_json::json!(target));
        }
        if let Some(flow) = self.add_space.as_mut() {
            flow.onboarding = Some(slug.clone().into());
            flow.error = None;
        }
        cx.notify();
        self.repo_task_set(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::ONBOARD_REPO, params).await;
            this.update(cx, |shell, cx| {
                if let Some(flow) = shell.add_space.as_mut() {
                    flow.onboarding = None;
                }
                match result
                    .map_err(|e| format!("{e}"))
                    .and_then(|v| serde_json::from_value::<comet_board::onboard::Onboarded>(v)
                        .map_err(|e| format!("Unreadable reply: {e}")))
                {
                    Ok(done) => {
                        // The space was created on the box and will arrive with
                        // the next workspace frame; echo it so landing in it is
                        // instant, exactly as `submit_add_space` does for a
                        // space this device created (same-id upsert is
                        // idempotent).
                        let space = Space {
                            id: done.space_id.clone(),
                            device_id: done.device_id.clone(),
                            path: done.path.clone(),
                            name: None,
                            git_detected: true,
                            git_checked_at: None,
                            checkout_id: None,
                            created_at: Utc::now(),
                        };
                        shell.state.update(cx, |s, cx| {
                            if !s.spaces.iter().any(|existing| existing.id == space.id) {
                                s.spaces.push(space);
                            }
                            cx.notify();
                        });
                        shell.add_space = None;
                        shell.activate_space(done.space_id, cx);
                    }
                    Err(message) => {
                        if let Some(flow) = shell.add_space.as_mut() {
                            flow.error = Some(message.into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// The current listing's folder rows filtered by the search query
    /// (prefix matches first — `popover::filter_indices`).
    fn add_space_filtered(&self, cx: &App) -> Vec<comet_proto::FolderEntry> {
        let Some(flow) = self.add_space.as_ref() else {
            return Vec::new();
        };
        let Some(listing) = flow.browser.ready() else {
            return Vec::new();
        };
        let dirs = browser_rows(listing);
        let query = flow.search.read(cx).text().to_string();
        let names: Vec<&str> = dirs.iter().map(|e| e.name.as_str()).collect();
        popover::filter_indices(&query, &names)
            .into_iter()
            .map(|ix| dirs[ix].clone())
            .collect()
    }

    /// Descend into the highlighted (filtered) folder; clears the query.
    fn add_space_open_active(&mut self, cx: &mut Context<Self>) {
        let rows = self.add_space_filtered(cx);
        let Some(flow) = self.add_space.as_ref() else {
            return;
        };
        let Some(listing) = flow.browser.ready() else {
            return;
        };
        let Some(entry) = rows.get(flow.active) else {
            return;
        };
        let full = crate::pickers::child_path(&listing.path, &entry.name);
        let is_repo = entry.is_repo;
        let search = flow.search.clone();
        if let Some(flow) = self.add_space.as_mut() {
            flow.browser_repo = is_repo;
        }
        search.update(cx, |input, cx| input.set_text("", cx));
        self.load_space_folders(Some(full), cx);
    }

    /// Descend into a specific folder row (mouse path); clears the query.
    fn add_space_descend(&mut self, full: String, is_repo: bool, cx: &mut Context<Self>) {
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        flow.browser_repo = is_repo;
        let search = flow.search.clone();
        search.update(cx, |input, cx| input.set_text("", cx));
        self.load_space_folders(Some(full), cx);
    }

    /// ListFolders on the flow's device (relay-forwarded when remote).
    pub(super) fn load_space_folders(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let local = self.state.read(cx).local_device_id.clone();
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        let device_id = flow.device.as_ref().map(|d| d.id.clone());
        let went_home = path.is_none();
        flow.browser_path = path.clone();
        flow.browser = Loadable::Loading;
        flow.active = 0;
        flow.list_scroll.set_offset(gpui::Point::default());
        flow.load_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            if let Some(p) = &path {
                params.insert("path".into(), serde_json::Value::String(p.clone()));
            }
            // Only target remote devices — local calls skip the relay.
            if let (Some(target), local) = (&device_id, &local)
                && local.as_deref() != Some(target.as_str())
            {
                params.insert(
                    "targetDeviceId".into(),
                    serde_json::Value::String(target.clone()),
                );
            }
            let result = engine
                .client()
                .call(methods::LIST_FOLDERS, serde_json::Value::Object(params))
                .await;
            this.update(cx, |shell, cx| {
                if let Some(flow) = shell.add_space.as_mut() {
                    flow.browser = match result {
                        Ok(value) => match serde_json::from_value::<FolderListing>(value) {
                            Ok(listing) => {
                                // A pathless browse resolved home — remember it
                                // so the breadcrumbs can fold it into the
                                // device crumb.
                                if went_home {
                                    flow.home = Some(listing.path.clone());
                                }
                                Loadable::Ready(listing)
                            }
                            Err(err) => Loadable::Error(err.to_string()),
                        },
                        Err(err) => Loadable::Error(err.to_string()),
                    };
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Create the space for the browser's current folder.
    fn submit_add_space(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(flow) = self.add_space.as_ref() else {
            return;
        };
        if flow.submit_busy {
            return;
        }
        let Some(device) = flow.device.clone() else {
            return;
        };
        let Some(listing) = flow.browser.ready() else {
            return;
        };
        let path = listing.path.clone();
        let git_detected = flow.browser_repo;
        // Same (device, folder) already has a space → just switch to it. The
        // engine dedupes this case too (a createSpace for a duplicate pair
        // no-ops), so creating would leave the minted id dangling.
        if let Some(existing) = self
            .state
            .read(cx)
            .spaces
            .iter()
            .find(|s| s.device_id == device.id && s.path == path)
            .map(|s| s.id.clone())
        {
            self.add_space = None;
            self.activate_space(existing, cx);
            return;
        }
        let Some(flow) = self.add_space.as_mut() else {
            return;
        };
        flow.submit_busy = true;
        flow.error = None;
        let space_id = uuid::Uuid::new_v4().to_string();
        // Optimistic echo: the watch frame carrying the real row replaces it
        // by id (apply_spaces re-sorts; same-id upsert is idempotent).
        let space = Space {
            id: space_id.clone(),
            device_id: device.id.clone(),
            path: path.clone(),
            name: None,
            git_detected,
            git_checked_at: None,
            checkout_id: None,
            created_at: Utc::now(),
        };
        self.state.update(cx, |s, cx| {
            if !s.spaces.iter().any(|existing| existing.id == space.id) {
                s.spaces.push(space);
            }
            cx.notify();
        });
        let params = serde_json::json!({
            "op": "createSpace",
            "spaceId": space_id,
            "deviceId": device.id,
            "path": path,
            "gitDetected": git_detected,
        });
        let submit_id = space_id.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::MUTATE, params).await;
            this.update(cx, |shell, cx| {
                match result {
                    Ok(_) => {
                        shell.add_space = None;
                        shell.activate_space(submit_id.clone(), cx);
                    }
                    Err(err) => {
                        // Roll the optimistic row back; surface the error inline.
                        shell.state.update(cx, |s, cx| {
                            s.spaces.retain(|space| space.id != submit_id);
                            cx.notify();
                        });
                        if let Some(flow) = shell.add_space.as_mut() {
                            flow.submit_busy = false;
                            flow.error = Some(format!("{err}").into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        });
        if let Some(flow) = self.add_space.as_mut() {
            flow.submit_task = Some(task);
        }
        cx.notify();
    }

    /// Go up to the parent folder (←, and ⌫ on an empty query).
    fn add_space_go_up(&mut self, cx: &mut Context<Self>) {
        let parent = self
            .add_space
            .as_ref()
            .and_then(|f| f.browser.ready())
            .and_then(|l| parent_path(&l.path));
        if let Some(parent) = parent {
            if let Some(flow) = self.add_space.as_mut() {
                flow.browser_repo = false; // unknown at the parent
            }
            self.load_space_folders(Some(parent), cx);
        }
    }

    /// Palette keys (bubbling from the focused search input) — every legend
    /// maps to a REAL key.
    ///
    /// In the repo list: ↑↓ navigate, ⏎ opens the highlighted repo (or connects
    /// it), → is the second door (browse folders), esc closes.
    ///
    /// In the folder browser: ↑↓ navigate, →/⏎ open the highlighted folder,
    /// ← up a level, ⌘⏎ add the OPEN folder, ⌫ (empty query) also goes up,
    /// esc closes.
    fn add_space_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        let mode = match self.add_space.as_ref() {
            Some(flow) => flow.mode,
            None => return,
        };
        // ←/→ act on the LIST, not the text cursor — the palette is a navigator
        // first; queries are short and edited with ⌫.
        match (event.keystroke.key.as_str(), mode) {
            ("right", AddSpaceMode::Folders) => {
                self.add_space_open_active(cx);
                return;
            }
            ("left", AddSpaceMode::Folders) => {
                self.add_space_go_up(cx);
                return;
            }
            // From the repo list, → is the way through to the other door and ←
            // is the way back: the two lists sit side by side, not one inside
            // the other.
            ("right", AddSpaceMode::Repos) => {
                self.add_space_set_mode(AddSpaceMode::Folders, cx);
                return;
            }
            _ => {}
        }
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        match key {
            popover::MenuKey::Escape => {
                self.add_space = None;
                cx.notify();
            }
            popover::MenuKey::Up | popover::MenuKey::Down => {
                let count = match mode {
                    AddSpaceMode::Repos => self.add_space_repo_rows(cx).len(),
                    AddSpaceMode::Folders => self.add_space_filtered(cx).len(),
                };
                let delta = if key == popover::MenuKey::Up { -1 } else { 1 };
                if let Some(flow) = self.add_space.as_mut() {
                    flow.active =
                        popover::menu_step(Some(flow.active), count, delta).unwrap_or(0);
                    // Keep the highlighted row in view as the cursor walks
                    // past the viewport (user-reported: the list didn't
                    // follow the keyboard).
                    flow.list_scroll.scroll_to_item(flow.active);
                    cx.notify();
                }
            }
            // Repo list: ⏎ IS the verb — open the space, or connect the repo.
            // There is no second step to reach for, which is the whole point.
            //
            // Folder browser: ⏎ opens the highlighted folder (an alias for →);
            // the space is added with ⌘⏎ — and the chord acts on the folder OPEN
            // in the breadcrumbs, not the highlight. The highlight auto-rests on
            // the first row, so a chord that took it would add arbitrary
            // subfolders; the usual target (a repo root full of subfolders) is
            // only ever "the folder you're standing in".
            popover::MenuKey::Enter => match mode {
                AddSpaceMode::Repos => self.add_space_activate_repo(cx),
                AddSpaceMode::Folders => self.add_space_open_active(cx),
            },
            popover::MenuKey::ModEnter if mode == AddSpaceMode::Folders => {
                self.submit_add_space(cx)
            }
            popover::MenuKey::ModEnter => {}
            popover::MenuKey::Backspace => {
                let empty = self
                    .add_space
                    .as_ref()
                    .is_some_and(|f| f.search.read(cx).is_empty());
                if empty {
                    match mode {
                        // ⌫ on an empty query is "back", and back from the
                        // folder browser is the repo list.
                        AddSpaceMode::Repos => {}
                        AddSpaceMode::Folders => {
                            let at_root = self
                                .add_space
                                .as_ref()
                                .and_then(|f| f.browser.ready())
                                .and_then(|l| parent_path(&l.path))
                                .is_none();
                            if at_root {
                                self.add_space_set_mode(AddSpaceMode::Repos, cx);
                            } else {
                                self.add_space_go_up(cx);
                            }
                        }
                    }
                }
            }
            popover::MenuKey::Other => {}
        }
    }

    /// The palette card: ⌘K search bar · the open door's list beside its rail ·
    /// kbd-hint footer.
    ///
    /// Two doors, one card (gh#118). The chrome is deliberately identical
    /// between them — same bar, same list rhythm, same rail column — because
    /// they are two views of one question, and a browser that looked like a
    /// different screen would make "add a space" feel like two features again.
    pub(super) fn render_add_space_overlay(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        {
            let flow = self.add_space.as_mut()?;
            if std::mem::take(&mut flow.focus_pending) {
                let handle = flow.search.focus_handle(cx);
                window.focus(&handle, cx);
            }
        }
        let (mode, search, error, active, focus, list_scroll, onboarding) = {
            let flow = self.add_space.as_ref()?;
            (
                flow.mode,
                flow.search.clone(),
                flow.error.clone(),
                flow.active,
                flow.focus.clone(),
                flow.list_scroll.clone(),
                flow.onboarding.clone(),
            )
        };
        let hairline = theme.white_alpha(0.06);

        // A quiet mono key-cap chip ("⌘K" / "esc") for the search bar ends.
        let key_chip = |theme: &Theme| {
            div()
                .h(px(22.0))
                .px(px(6.0))
                .rounded(px(5.0))
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(2.0))
                .bg(theme.white_alpha(0.05))
                .text_size(px(11.0))
                .font_family(theme.font_mono.clone())
                .text_color(theme.text_muted.opacity(0.7))
        };

        // ── search bar (the ⌘K bar): summon chip · input · primary chip · esc.
        //    The primary chip names what ⏎ (or ⌘⏎) will actually do to the row
        //    under the cursor, because in the repo list those are two different
        //    verbs — opening a space you have, and cloning one you do not.
        let (primary_label, primary_enabled): (SharedString, bool) = match mode {
            AddSpaceMode::Repos => match &onboarding {
                Some(slug) => (format!("Connecting {slug}…").into(), false),
                None => {
                    let rows = self.add_space_repo_rows(cx);
                    match rows.get(active) {
                        Some(row) if row.connected() => ("Open".into(), true),
                        Some(_) => ("Connect".into(), true),
                        None => ("Open".into(), false),
                    }
                }
            },
            AddSpaceMode::Folders => {
                let flow = self.add_space.as_ref()?;
                if flow.submit_busy {
                    ("Adding…".into(), false)
                } else {
                    ("Enter".into(), flow.browser.ready().is_some())
                }
            }
        };
        let busy = matches!(mode, AddSpaceMode::Repos) && onboarding.is_some()
            || matches!(mode, AddSpaceMode::Folders)
                && self.add_space.as_ref().is_some_and(|f| f.submit_busy);
        let submit_chip = popover::btn_primary(&theme, "")
            .id("add-space-submit")
            .h(px(22.0))
            .px(px(8.0))
            .py(px(0.0))
            // Match the key-cap chips beside it (rounded-5) — btn_primary's
            // rounded-8 at this size read as a different component.
            .rounded(px(5.0))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .text_size(px(12.0))
            .when(!primary_enabled, |el| el.opacity(0.6))
            .on_click(cx.listener(move |this, _, _, cx| {
                let mode = this.add_space.as_ref().map(|f| f.mode);
                match mode {
                    Some(AddSpaceMode::Repos) => this.add_space_activate_repo(cx),
                    Some(AddSpaceMode::Folders) => this.submit_add_space(cx),
                    None => {}
                }
            }))
            // The chip shows the key that really does this: ⏎ alone in the repo
            // list (the row under the cursor IS the target), ⌘⏎ in the folder
            // browser (where plain ⏎ descends instead).
            .when(!busy, |el| {
                el.child(
                    icon(if mode == AddSpaceMode::Repos {
                        icons::RETURN
                    } else {
                        icons::COMMAND
                    })
                    .size(px(11.0))
                    .text_color(theme.bg.opacity(0.8)),
                )
            })
            .child(primary_label.clone());
        // Header and footer sit a shade DEEPER than the body (the shared
        // recessed-band tone) — the bands frame the list, which stays on the
        // brighter tint.
        let band = popover::band(&theme);
        let input_row = div()
            .h(px(46.0))
            .flex_none()
            .pl(px(12.0))
            .pr(px(10.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .bg(band)
            .border_b_1()
            .border_color(hairline)
            .child(
                key_chip(&theme)
                    .child(icon(icons::COMMAND).size(px(11.0)).text_color(theme.text_muted.opacity(0.7)))
                    .child(SharedString::from("K")),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(14.0))
                    .child(search.clone().into_any_element()),
            )
            .child(submit_chip)
            .child(
                key_chip(&theme)
                    .id("add-space-esc")
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.white_alpha(0.09)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.add_space = None;
                        cx.notify();
                    }))
                    .child(SharedString::from("esc")),
            );

        let (column, rail, hints) = match mode {
            AddSpaceMode::Repos => (
                self.render_repo_column(&theme, active, &list_scroll, cx),
                self.render_repo_rail(&theme, cx),
                vec![
                    popover::key_hint_pair(&theme, icons::ARROW_UP, icons::ARROW_DOWN, "Navigate"),
                    popover::key_hint(&theme, icons::RETURN, "Open"),
                    popover::key_hint(&theme, icons::ARROW_RIGHT, "Browse folders"),
                ],
            ),
            AddSpaceMode::Folders => (
                self.render_folder_column(&theme, active, &list_scroll, cx)?,
                self.render_devices_rail(&theme, cx),
                vec![
                    popover::key_hint_pair(&theme, icons::ARROW_UP, icons::ARROW_DOWN, "Navigate"),
                    popover::key_hint(&theme, icons::ARROW_LEFT, "Up"),
                    popover::key_hint(&theme, icons::ARROW_RIGHT, "Open"),
                ],
            ),
        };

        // ── body: the list column beside its rail. FIXED height — sparse lists,
        //    loading skeletons, and door switches must not resize the card (the
        //    list fills and scrolls).
        let body = div()
            .h(px(330.0))
            .flex()
            .flex_row()
            .items_stretch()
            .child(div().flex_1().min_w_0().flex().flex_col().child(column))
            .child(rail);

        // ── footer: the shared key-cap legend voice (popover::key_hint).
        let footer = div()
            .flex_none()
            .bg(band)
            .border_t_1()
            .border_color(hairline)
            .px(px(12.0))
            .py(px(8.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .children(hints)
            .when_some(error, |el, message| {
                el.child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_size(px(11.0))
                        .text_color(theme.danger)
                        .child(message),
                )
            });

        let card = div()
            .id("add-space-palette")
            .w(px(680.0))
            .rounded(px(14.0))
            .border_1()
            .border_color(theme.white_alpha(0.10))
            // The popover_card glass recipe: a translucent tint over the
            // frosted backdrop blur (`popover::modal` wraps in `frosted`) —
            // an opaque fill here killed the vibrancy every other float has.
            .bg(theme.float_card())
            .shadow_lg()
            .overflow_hidden()
            .flex()
            .flex_col()
            .text_color(theme.text)
            // On the keyboard dispatch path (see `AddSpaceFlow::focus`) — the
            // pickers' proven structure for frame-level keys with a focused
            // child input.
            .track_focus(&focus)
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                this.add_space_key(event, cx)
            }))
            // Clicking the scrim dismisses (user requirement) — same close
            // path as Escape.
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.add_space = None;
                cx.notify();
            }))
            .child(input_row)
            .child(body)
            .child(footer)
            .into_any_element();
        Some(popover::modal("add-space-dialog", viewport, card))
    }

    // ---- the repo door ----

    /// The repo list: one row per space (named by its repo where the box could
    /// say), then the repos with no space yet.
    fn render_repo_column(
        &mut self,
        theme: &Theme,
        active: usize,
        list_scroll: &gpui::ScrollHandle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rows = self.add_space_repo_rows(cx);
        let (sweeping, onboarding) = match self.add_space.as_ref() {
            Some(flow) => (
                matches!(flow.repos, Loadable::Loading | Loadable::Idle),
                flow.onboarding.clone(),
            ),
            None => (false, None),
        };
        let query_empty = self
            .add_space
            .as_ref()
            .is_some_and(|f| f.search.read(cx).is_empty());
        let now = Utc::now();
        let device_labels: std::collections::HashMap<String, (String, bool)> = {
            let state = self.state.read(cx);
            state
                .devices
                .iter()
                .map(|d| {
                    (
                        d.id.clone(),
                        (d.name.clone(), state.device_online(&d.id, now)),
                    )
                })
                .collect()
        };

        if rows.is_empty() {
            return if sweeping {
                div()
                    .px(px(8.0))
                    .py(px(10.0))
                    .child(popover::skeleton_rows("add-space-repo-skeleton", theme, 6))
                    .into_any_element()
            } else {
                div()
                    .px(px(14.0))
                    .py(px(16.0))
                    .text_size(px(12.5))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(if query_empty {
                        // Nothing to pick and nothing wrong: this is a fresh
                        // install with no board yet, and the second door is the
                        // honest answer rather than an error.
                        "No repos yet — browse this device's folders instead (→)"
                    } else {
                        "No repos match"
                    }))
                    .into_any_element()
            };
        }

        // The 6px gutters live on a WRAPPER, outside the scroll viewport: see
        // the folder list's note — the wheel's max offset eats bottom padding.
        div()
            .flex_1()
            .min_h_0()
            .py(px(6.0))
            .child(
                div()
                    .id("add-space-repos")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(list_scroll)
                    .px(px(8.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .children(rows.into_iter().enumerate().map(|(ix, row)| {
                        // `is_some()` first: two `None`s are equal, and without
                        // it every space that is not a GitHub checkout would
                        // spin forever.
                        let connecting =
                            onboarding.is_some() && onboarding.as_deref() == row.slug.as_deref();
                        self.render_repo_row(theme, ix, ix == active, connecting, &device_labels, row, cx)
                    })),
            )
            .into_any_element()
    }

    /// One repo row: mark, name, and what is true about it. Click opens the
    /// space, or starts the clone.
    #[allow(clippy::too_many_arguments)]
    fn render_repo_row(
        &self,
        theme: &Theme,
        ix: usize,
        highlighted: bool,
        connecting: bool,
        devices: &std::collections::HashMap<String, (String, bool)>,
        row: RepoRow,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let title: SharedString = row.title.clone().into();
        let note = row.note();
        let connected = row.connected();
        // A connected row names its device the way the sidebar's space rows do —
        // and marks it offline, because a space whose host is down is a space you
        // cannot start anything in.
        let device_tag = row.device_id.as_ref().and_then(|id| {
            devices.get(id).map(|(name, online)| {
                if *online {
                    format!("@ {name}")
                } else {
                    format!("@ {name} · offline")
                }
            })
        });
        let device_offline = row
            .device_id
            .as_ref()
            .and_then(|id| devices.get(id))
            .is_some_and(|(_, online)| !online);
        let picked = row.clone();
        popover::menu_row_nav(theme, false, highlighted, format!("add-space-repo-{ix}"))
            .when(highlighted, |el| el.shadow(theme.glass_selected_shadows()))
            .id(("add-space-repo", ix))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.add_space_pick_repo(picked.clone(), cx);
            }))
            .child(
                div()
                    .w(px(16.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(if connecting {
                        loaders::mini_gradient_spinner(format!("add-space-connecting-{ix}"), 2.0)
                            .into_any_element()
                    } else if row.slug.is_some() {
                        icon(icons::GITHUB_MARK)
                            .size(px(14.0))
                            .text_color(theme.text_muted.opacity(if connected {
                                0.85
                            } else {
                                // An unconnected repo is a row that will make you
                                // wait; it sits a shade quieter than the ones
                                // that open instantly.
                                0.5
                            }))
                            .into_any_element()
                    } else {
                        icon(icons::FOLDER)
                            .size(px(15.0))
                            .text_color(theme.text_muted.opacity(0.8))
                            .into_any_element()
                    }),
            )
            .child(div().flex_1().min_w_0().truncate().child(title))
            .when_some(note, |el, note| {
                el.child(
                    div()
                        .flex_none()
                        .text_size(px(11.0))
                        .text_color(theme.text_muted.opacity(0.55))
                        .child(SharedString::from(note)),
                )
            })
            .when_some(device_tag, |el, tag| {
                el.child(
                    div()
                        .flex_none()
                        .text_size(px(11.5))
                        .text_color(if device_offline {
                            theme.warning.opacity(0.8)
                        } else {
                            theme.text_muted.opacity(0.6)
                        })
                        .child(SharedString::from(tag)),
                )
            })
            .into_any_element()
    }

    /// The repo door's rail: where a new repo would land, and the way to the
    /// folder browser.
    fn render_repo_rail(&mut self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let hairline = theme.white_alpha(0.06);
        let now = Utc::now();
        let flow = self.add_space.as_ref();
        // The device ids of every board the sweep found, in sweep order.
        let hosts: Vec<String> = flow
            .map(|f| f.hosts.iter().map(|h| h.device_id.clone()).collect())
            .unwrap_or_default();
        let target = flow.map(|f| f.target).unwrap_or(0);
        let sweeping = flow.is_some_and(|f| matches!(f.repos, Loadable::Loading | Loadable::Idle));
        let note = flow.and_then(|f| f.host()).and_then(|h| h.note.clone());
        let (names, presence): (
            std::collections::HashMap<String, String>,
            std::collections::HashMap<String, bool>,
        ) = {
            let state = self.state.read(cx);
            (
                state
                    .devices
                    .iter()
                    .map(|d| (d.id.clone(), d.name.clone()))
                    .collect(),
                state
                    .devices
                    .iter()
                    .map(|d| (d.id.clone(), state.device_online(&d.id, now)))
                    .collect(),
            )
        };
        let host_name = |device_id: &str| -> String {
            names
                .get(device_id)
                .cloned()
                .unwrap_or_else(|| "the board's device".to_string())
        };
        // One host is not a choice, so it is not offered as one: the box is the
        // default target and the rail simply says so (gh#118). Two is a choice
        // nothing here can make for you.
        let asking = hosts.len() > 1;

        let mut rail = div()
            .w(px(196.0))
            .flex_none()
            .border_l_1()
            .border_color(hairline)
            .px(px(8.0))
            .py(px(8.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(
                div()
                    .px(px(8.0))
                    .pt(px(2.0))
                    .pb(px(4.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .text_size(px(11.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted.opacity(0.6))
                    .child(SharedString::from(if asking {
                        "Clone onto"
                    } else {
                        "Board"
                    }))
                    .when(sweeping, |el| {
                        el.child(loaders::mini_gradient_spinner("add-space-sweep", 2.0))
                    }),
            );

        for (ix, device_id) in hosts.iter().enumerate() {
            let selected = asking && ix == target;
            let online = presence.get(device_id).copied().unwrap_or(false);
            let name: SharedString = host_name(device_id).into();
            rail = rail.child(
                div()
                    .id(("add-space-host", ix))
                    .h(px(28.0))
                    .px(px(8.0))
                    .rounded(px(8.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(px(12.5))
                    .when(selected, |el| {
                        el.bg(theme.glass_selected_bg())
                            .shadow(theme.glass_selected_shadows())
                            .text_color(theme.text)
                    })
                    .when(!selected, |el| el.text_color(theme.text_muted.opacity(0.8)))
                    .when(asking, |el| {
                        el.cursor_pointer()
                            .hover(|s| s.bg(theme.element_hover))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(flow) = this.add_space.as_mut() {
                                    flow.target = ix;
                                }
                                cx.notify();
                            }))
                    })
                    .child(
                        icon(icons::MONITOR)
                            .size(px(14.0))
                            .flex_none()
                            .text_color(theme.text_muted.opacity(0.8)),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(name))
                    .child(
                        div()
                            .size(px(5.0))
                            .rounded_full()
                            .flex_none()
                            .when(online, |el| {
                                let emerald = crate::theme::oklch(0.765, 0.177, 163.223);
                                el.bg(emerald.opacity(0.9))
                            })
                            .when(!online, |el| el.bg(theme.white_alpha(0.22))),
                    ),
            );
        }

        let info: SharedString = if sweeping {
            "Looking for the board…".into()
        } else if hosts.is_empty() {
            "No device here hosts a board, so a repo has nowhere to be cloned.".into()
        } else if asking {
            "Pick where a new repo gets cloned.".into()
        } else {
            format!(
                "New repos clone onto {} and go on the board.",
                host_name(&hosts[0])
            )
            .into()
        };
        rail = rail
            .child(div().h(px(1.0)).mx(px(2.0)).my(px(6.0)).bg(hairline))
            .child(
                div()
                    .px(px(8.0))
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(px(6.0))
                    .text_size(px(11.0))
                    .line_height(px(15.0))
                    .text_color(theme.text_muted.opacity(0.5))
                    .child(
                        icon(icons::INFO_CIRCLE)
                            .size(px(12.0))
                            .flex_none()
                            .mt(px(1.0))
                            .text_color(theme.text_muted.opacity(0.5)),
                    )
                    .child(div().min_w_0().child(info)),
            );
        // Why the App offered nothing, when it offered nothing. Not an error: a
        // board on a `GITHUB_TOKEN` has no installations to enumerate (gh#97),
        // and its spaces are on the list above regardless.
        if let Some(note) = note {
            rail = rail.child(
                div()
                    .px(px(8.0))
                    .pt(px(6.0))
                    .text_size(px(11.0))
                    .line_height(px(15.0))
                    .text_color(theme.text_muted.opacity(0.45))
                    .child(note),
            );
        }

        rail.child(div().flex_1())
            .child(div().h(px(1.0)).mx(px(2.0)).my(px(6.0)).bg(hairline))
            .child(
                div()
                    .px(px(8.0))
                    .pb(px(4.0))
                    .text_size(px(11.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted.opacity(0.6))
                    .child(SharedString::from("This device")),
            )
            // The second door. A scratch folder that is nobody's repo is a real
            // place to work, and the repo list cannot offer it.
            .child(
                div()
                    .id("add-space-browse")
                    .h(px(28.0))
                    .px(px(8.0))
                    .rounded(px(8.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(px(12.5))
                    .text_color(theme.text_muted.opacity(0.8))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.element_hover))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.add_space_set_mode(AddSpaceMode::Folders, cx);
                    }))
                    .child(
                        icon(icons::FOLDER)
                            .size(px(14.0))
                            .flex_none()
                            .text_color(theme.text_muted.opacity(0.8)),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(SharedString::from(
                        "Browse folders…",
                    )))
                    .child(
                        icon(icons::ARROW_RIGHT)
                            .size(px(12.0))
                            .flex_none()
                            .text_color(theme.text_faint.opacity(0.7)),
                    ),
            )
    }

    // ---- the folder door ----

    /// Breadcrumbs + folder list (with a crumb back to the repo list).
    fn render_folder_column(
        &mut self,
        theme: &Theme,
        active: usize,
        list_scroll: &gpui::ScrollHandle,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (device, loading, load_error, listing, home) = {
            let flow = self.add_space.as_ref()?;
            (
                flow.device.clone(),
                matches!(flow.browser, Loadable::Loading | Loadable::Idle),
                flow.browser.error().map(str::to_string),
                flow.browser.ready().cloned(),
                flow.home.clone(),
            )
        };
        let rows = self.add_space_filtered(cx);
        let query_empty = self
            .add_space
            .as_ref()
            .is_some_and(|f| f.search.read(cx).is_empty());
        let device_name: SharedString = device
            .as_ref()
            .map(|d| d.name.clone())
            .unwrap_or_else(|| "This device".to_string())
            .into();

        // ── breadcrumbs ("Repos / MacBook Pro / Projects / comet"): the quiet
        //    mono path voice, `/` separators. The leading crumb is the way back
        //    to the repo list — the folder browser is a room off it, and every
        //    room needs a door. The device crumb stands in for home: everything
        //    up to the resolved home path folds into it; below home the full
        //    path shows. Ancestors are clickable.
        let back_crumb = div()
            .id("add-space-crumb-repos")
            .px(px(3.0))
            .rounded(px(4.0))
            .text_color(theme.text_muted.opacity(0.55))
            .cursor_pointer()
            .hover(|s| s.text_color(theme.text))
            .on_click(cx.listener(|this, _, _, cx| {
                this.add_space_set_mode(AddSpaceMode::Repos, cx);
            }))
            .child(SharedString::from("Repos"));
        let separator = |theme: &Theme| {
            div()
                .text_color(theme.text_faint.opacity(0.7))
                .child(SharedString::from("/"))
        };
        let crumbs: AnyElement = match &listing {
            Some(listing) => {
                let segments = breadcrumbs(&listing.path);
                let last = segments.len().saturating_sub(1);
                // Root "/" chip always folds; the home segments fold too when
                // the browsed path sits at/under home.
                let at_home = home.as_deref() == Some(listing.path.as_str());
                let folded = 1 + home
                    .as_deref()
                    .filter(|h| {
                        listing.path == *h || listing.path.starts_with(&format!("{h}/"))
                    })
                    .map(|h| h.split('/').filter(|s| !s.is_empty()).count())
                    .unwrap_or(0);
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .px(px(13.0))
                    .pt(px(10.0))
                    .pb(px(2.0))
                    .text_size(px(11.0))
                    .font_family(theme.font_mono.clone())
                    .child(back_crumb)
                    .child(separator(theme))
                    .child({
                        let crumb = div()
                            .id("add-space-crumb-device")
                            .px(px(3.0))
                            .rounded(px(4.0))
                            .child(device_name.clone());
                        if at_home {
                            // Standing at home — the device crumb IS the
                            // current folder.
                            crumb.text_color(theme.text.opacity(0.85)).into_any_element()
                        } else {
                            crumb
                                .text_color(theme.text_muted.opacity(0.55))
                                .cursor_pointer()
                                .hover(|s| s.text_color(theme.text))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(flow) = this.add_space.as_mut() {
                                        flow.browser_repo = false;
                                    }
                                    this.load_space_folders(None, cx);
                                }))
                                .into_any_element()
                        }
                    })
                    .children(
                        segments
                            .into_iter()
                            .enumerate()
                            .skip(folded)
                            .map(|(ix, (label, full))| {
                                let is_last = ix == last;
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .child(separator(theme))
                                    .child({
                                        let crumb = div()
                                            .id(("add-space-crumb", ix))
                                            .px(px(3.0))
                                            .rounded(px(4.0))
                                            .text_color(if is_last {
                                                theme.text.opacity(0.85)
                                            } else {
                                                theme.text_muted.opacity(0.55)
                                            })
                                            .child(SharedString::from(label));
                                        if is_last {
                                            crumb.into_any_element()
                                        } else {
                                            crumb
                                                .cursor_pointer()
                                                .hover(|s| s.text_color(theme.text))
                                                .on_click(cx.listener(
                                                    move |this, _, _, cx| {
                                                        if let Some(flow) =
                                                            this.add_space.as_mut()
                                                        {
                                                            flow.browser_repo = false;
                                                        }
                                                        this.load_space_folders(
                                                            Some(full.clone()),
                                                            cx,
                                                        );
                                                    },
                                                ))
                                                .into_any_element()
                                        }
                                    })
                            }),
                    )
                    .into_any_element()
            }
            None => div()
                .flex()
                .flex_row()
                .items_center()
                .px(px(13.0))
                .pt(px(10.0))
                .pb(px(2.0))
                .text_size(px(11.0))
                .font_family(theme.font_mono.clone())
                .child(back_crumb)
                .into_any_element(),
        };

        // ── folder list ─────────────────────────────────────────────────────
        let base_path = listing.as_ref().map(|l| l.path.clone()).unwrap_or_default();
        let list: AnyElement = if loading {
            div()
                .px(px(8.0))
                .py(px(6.0))
                .child(popover::skeleton_rows("add-space-skeleton", theme, 6))
                .into_any_element()
        } else if let Some(message) = load_error {
            let device_line = device
                .as_ref()
                .map(|d| format!("{} didn't respond — is it online?", d.name))
                .unwrap_or(message);
            popover::error_row(theme, &device_line)
                .px(px(14.0))
                .py(px(10.0))
                .child(
                    div()
                        .id("add-space-retry")
                        .px(px(Theme::SPACE_SM))
                        .py(px(3.0))
                        .rounded(px(Theme::CONTROL_RADIUS))
                        .border_1()
                        .border_color(theme.border)
                        .text_color(theme.text)
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.element_hover))
                        .on_click(cx.listener(|this, _, _, cx| {
                            let path = this.add_space.as_ref().and_then(|f| f.browser_path.clone());
                            this.load_space_folders(path, cx);
                        }))
                        .child(SharedString::from("Retry")),
                )
                .into_any_element()
        } else if rows.is_empty() {
            div()
                .px(px(14.0))
                .py(px(16.0))
                .text_size(px(12.5))
                .text_color(theme.text_faint)
                .child(SharedString::from(if query_empty {
                    "No folders here"
                } else {
                    "No folders match"
                }))
                .into_any_element()
        } else {
            // The 6px gutters live on a WRAPPER, outside the scroll viewport:
            // in-content padding/spacers can't do it — the wheel's max offset
            // eats bottom padding, and `scroll_to_item` (keyboard) pins the
            // row's bottom to the viewport edge regardless.
            div()
                .flex_1()
                .min_h_0()
                .py(px(6.0))
                .child(
                    div()
                        .id("add-space-folders")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(list_scroll)
                        .px(px(8.0))
                        .flex()
                        .flex_col()
                        // The app-wide list rhythm (sidebar rows, menu rows): 2px.
                        .gap(px(2.0))
                .children(rows.into_iter().enumerate().map(|(ix, entry)| {
                    let name: SharedString = entry.name.clone().into();
                    let full = crate::pickers::child_path(&base_path, &entry.name);
                    let is_repo = entry.is_repo;
                    popover::menu_row_nav(theme, false, ix == active, format!("add-space-folder-{ix}"))
                        // The active-tab/session selection language: the wash
                        // plus the ring-only inset outline.
                        .when(ix == active, |el| {
                            el.shadow(theme.glass_selected_shadows())
                        })
                        .id(("add-space-folder", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.add_space_descend(full.clone(), is_repo, cx);
                        }))
                        .child(
                            icon(icons::FOLDER)
                                .size(px(15.0))
                                .flex_none()
                                .text_color(theme.text_muted.opacity(0.8)),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(name))
                        // Repos get a quiet trailing branch glyph — the row
                        // you're usually hunting for announces itself.
                        .when(is_repo, |el| {
                            el.child(
                                icon(icons::GIT_BRANCH)
                                    .size(px(13.0))
                                    .flex_none()
                                    .text_color(theme.text_muted.opacity(0.5)),
                            )
                        })
                        })),
                )
                .into_any_element()
        };

        Some(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(crumbs)
                .child(list)
                .into_any_element(),
        )
    }

    /// The folder door's rail: platform glyph + name + presence dot per row, an
    /// info line naming the browsed device. Rows are the tab recipe (h-28
    /// rounded-8 washes), vertical.
    fn render_devices_rail(&mut self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let hairline = theme.white_alpha(0.06);
        let now = Utc::now();
        let devices = self.state.read(cx).devices.clone();
        let device = self.add_space.as_ref().and_then(|f| f.device.clone());
        let device_presence: Vec<bool> = {
            let state = self.state.read(cx);
            devices.iter().map(|d| state.device_online(&d.id, now)).collect()
        };
        let device_name: SharedString = device
            .as_ref()
            .map(|d| d.name.clone())
            .unwrap_or_else(|| "This device".to_string())
            .into();

        div()
            .w(px(196.0))
            .flex_none()
            .border_l_1()
            .border_color(hairline)
            .px(px(8.0))
            .py(px(8.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(
                div()
                    .px(px(8.0))
                    .pt(px(2.0))
                    .pb(px(4.0))
                    .text_size(px(11.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_muted.opacity(0.6))
                    .child(SharedString::from("Devices")),
            )
            .children(devices.into_iter().enumerate().map(|(ix, dev)| {
                let is_active = device.as_ref().is_some_and(|d| d.id == dev.id);
                let online = device_presence.get(ix).copied().unwrap_or(false);
                // The Devices-page platform mapping (settings::devices).
                let platform_icon = match dev.platform.as_str() {
                    "macos" | "darwin" => icons::LAPTOP,
                    "web" => icons::GLOBAL,
                    "ios" | "android" => icons::SMARTPHONE,
                    _ => icons::MONITOR,
                };
                let name: SharedString = dev.name.clone().into();
                let pick = dev.clone();
                div()
                    .id(("add-space-device", ix))
                    .h(px(28.0))
                    .px(px(8.0))
                    .rounded(px(8.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(px(12.5))
                    .cursor_pointer()
                    .when(is_active, |el| {
                        // The sidebar's selection language: glass wash +
                        // ring-only inset outline.
                        el.bg(theme.glass_selected_bg())
                            .shadow(theme.glass_selected_shadows())
                            .text_color(theme.text)
                    })
                    .when(!is_active, |el| {
                        el.text_color(theme.text_muted.opacity(0.7))
                            .hover(|s| s.bg(theme.element_hover))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.add_space_pick_device(pick.clone(), cx);
                    }))
                    .child(
                        icon(platform_icon)
                            .size(px(14.0))
                            .flex_none()
                            .text_color(theme.text_muted.opacity(0.8)),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(name))
                    .child(
                        div().size(px(5.0)).rounded_full().flex_none().when(online, |el| {
                            // The Devices-page presence emerald, soft glow
                            // included.
                            let emerald = crate::theme::oklch(0.765, 0.177, 163.223);
                            el.bg(emerald.opacity(0.9)).shadow(vec![gpui::BoxShadow {
                                color: emerald.opacity(0.55),
                                offset: gpui::point(px(0.0), px(0.0)),
                                blur_radius: px(6.0),
                                spread_radius: px(0.0),
                                inset: false,
                            }])
                        })
                        .when(!online, |el| el.bg(theme.white_alpha(0.22))),
                    )
            }))
            .child(div().h(px(1.0)).mx(px(2.0)).my(px(6.0)).bg(hairline))
            .child(
                div()
                    .px(px(8.0))
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(px(6.0))
                    .text_size(px(11.0))
                    .line_height(px(15.0))
                    .text_color(theme.text_muted.opacity(0.5))
                    .child(
                        icon(icons::INFO_CIRCLE)
                            .size(px(12.0))
                            .flex_none()
                            .mt(px(1.0))
                            .text_color(theme.text_muted.opacity(0.5)),
                    )
                    .child(div().min_w_0().child(SharedString::from(format!(
                        "Showing folders from {device_name} only"
                    )))),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("add-space-back-to-repos")
                    .h(px(28.0))
                    .px(px(8.0))
                    .rounded(px(8.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(px(12.5))
                    .text_color(theme.text_muted.opacity(0.8))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.element_hover))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.add_space_set_mode(AddSpaceMode::Repos, cx);
                    }))
                    .child(
                        icon(icons::ARROW_LEFT)
                            .size(px(12.0))
                            .flex_none()
                            .text_color(theme.text_faint.opacity(0.7)),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(SharedString::from(
                        "Back to repos",
                    ))),
            )
    }

    // ---- space context menu / rename / delete overlays ----

    pub(super) fn open_rename_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.space_menu = None;
        let current = self
            .state
            .read(cx)
            .space_row(&space_id)
            .map(|s| s.display_name().to_string())
            .unwrap_or_default();
        let input = cx.new(|cx| ComposerInput::new("Space name", cx));
        input.update(cx, |input, cx| input.set_text(current, cx));
        let events = cx.subscribe(&input, |this: &mut Shell, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_rename_space(cx);
            }
        });
        self.rename_space_dialog = Some(RenameSpaceDialog {
            space_id,
            input,
            focus_pending: true,
            _events: events,
        });
        cx.notify();
    }

    pub(super) fn submit_rename_space(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.rename_space_dialog.take() else {
            return;
        };
        let name = dialog.input.read(cx).text().trim().to_string();
        if !name.is_empty() {
            self.mutate(
                serde_json::json!({ "op": "renameSpace", "spaceId": dialog.space_id, "name": name }),
                cx,
            );
        }
        cx.notify();
    }

    pub(super) fn delete_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.delete_space_confirm = None;
        self.mutate(
            serde_json::json!({ "op": "deleteSpace", "spaceId": space_id }),
            cx,
        );
        cx.notify();
    }

    /// Space context menu + rename dialog + delete confirm (appended to the
    /// shell's overlay list).
    pub(super) fn render_space_overlays(
        &mut self,
        viewport: gpui::Size<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = Theme::of(cx).clone();
        let mut overlays: Vec<AnyElement> = Vec::new();

        if let Some((space_id, position)) = self.space_menu.clone() {
            let rename_id = space_id.clone();
            let delete_id = space_id.clone();
            let menu = popover::popover_card(&theme)
                .w(px(170.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.space_menu = None;
                    cx.notify();
                }))
                .flex()
                .flex_col()
                .child(
                    popover::menu_row(&theme, false, format!("space-menu-rename-{space_id}"))
                        .id("space-menu-rename")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_rename_space(rename_id.clone(), cx)
                        }))
                        .child(icon(icons::PEN).size(px(16.0)).text_color(theme.text_muted))
                        .child(SharedString::from("Rename…")),
                )
                .child(popover::menu_separator(&theme))
                .child(
                    popover::menu_row(&theme, false, format!("space-menu-delete-{space_id}"))
                        .id("space-menu-delete")
                        .text_color(theme.danger)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.space_menu = None;
                            this.delete_space_confirm = Some(delete_id.clone());
                            cx.notify();
                        }))
                        .child(
                            icon(icons::TRASH_BIN_MINIMALISTIC)
                                .size(px(16.0))
                                .text_color(theme.danger),
                        )
                        .child(SharedString::from("Remove…")),
                )
                .into_any_element();
            overlays.push(popover::menu_at("space-context-menu", position, menu));
        }

        if let Some(dialog) = &mut self.rename_space_dialog {
            if std::mem::take(&mut dialog.focus_pending) {
                window.focus(&dialog.input.focus_handle(cx), cx);
            }
            let input = dialog.input.clone();
            let card = popover::dialog_card(&theme)
                .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" {
                        this.rename_space_dialog = None;
                        cx.notify();
                    }
                }))
                .child(popover::dialog_title(&theme, "Rename space"))
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
                            popover::btn_ghost(&theme, "Cancel", "rename-space-cancel")
                                .id("rename-space-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.rename_space_dialog = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Rename")
                                .id("rename-space-save")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.submit_rename_space(cx)),
                                ),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("rename-space-dialog", viewport, card));
        }

        if let Some(space_id) = self.delete_space_confirm.clone() {
            let (name, device, count) = {
                let state = self.state.read(cx);
                let space = state.space_row(&space_id);
                (
                    space
                        .map(|s| s.display_name().to_string())
                        .unwrap_or_else(|| "this space".into()),
                    space
                        .and_then(|s| state.device_name(&s.device_id))
                        .unwrap_or("its device")
                        .to_string(),
                    state.chats_in_space(&space_id).len(),
                )
            };
            let copy = if count == 1 {
                format!(
                    "Removing “{name}” permanently deletes its 1 session on {device}. This can’t be undone."
                )
            } else {
                format!(
                    "Removing “{name}” permanently deletes its {count} sessions on {device}. This can’t be undone."
                )
            };
            let card = popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Remove space?"))
                .child(div().mt(px(6.0)).child(popover::dialog_body(&theme, copy)))
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "delete-space-cancel")
                                .id("delete-space-cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.delete_space_confirm = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_danger(&theme, "Remove")
                                .id("delete-space-confirm")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.delete_space(space_id.clone(), cx)
                                })),
                        ),
                )
                .into_any_element();
            overlays.push(popover::modal("delete-space-dialog", viewport, card));
        }

        overlays
    }
}
