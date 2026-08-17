//! The session tab strip — replaces the chat header (feature spec: spaces
//! overhaul). Every non-archived session of the selected space is a tab:
//! a leading 5px status dot + title, and — on the SELECTED tab only — a
//! trailing close button (`docs/design/window.md` claims B6/B7). `+` at the end
//! opens the new-session canvas (the tab materializes on first send). The strip
//! inherits the old header's titlebar duties: drag region, animated
//! window-controls inset, and the toggle-changes button (git spaces only).
//!
//! Since gh#124 the strip is IN-SPACE NAVIGATION, not a session switcher: the
//! sidebar's disclosed space rows are the authoritative session surface, and
//! this strip only walks the selected space's tabs (same order — see
//! [`Shell::tab_ids`]) while carrying the titlebar.
//!
//! ## A tab that is not a chat (gh#311)
//!
//! On [`Route::Review`] the strip leads with the REVIEW's own tab — the canvas
//! puts it there (`docs/design/canvas/comet-review-window.dc.html:44`), and it
//! is the answer to a review being a room you could only leave by knowing a
//! shortcut. So a tab is no longer necessarily a chat: this one is titled off
//! the route, wears the `--review` hue at rest instead of a session's status,
//! is pinned at the head (no drag order — it belongs to no space), and closing
//! it leaves the route. Everything after it is still the selected space's
//! chats, drawn unselected, because on this route the review is what is
//! selected; clicking one leaves the review for it.
//!
//! The lead tab occupies the first slot of the SAME scroll container as the
//! chats, so it scrolls and gaps with them — which means the two index maths in
//! here (drop slot, scroll-to-selected) run one slot in from the left. That is
//! `lead` below, and it is the whole cost of the second kind of tab.
//!
//! Styling and drag-reorder mirror the terminal tab bar
//! (`terminal/panel.rs::render_tab_bar`) — same fixed-width tabs, drop-index
//! math, 150ms sibling slide, and drag ghost. The manual order is device-local
//! (`UiSettings.tab_order`, keyed by space). Overflow scrolls horizontally
//! with edge fades.

use super::*;
use crate::motion::TAB_SLIDE;
use crate::terminal::panel::{drop_index, reorder_tabs, slide_offset};
use crate::theme::{Bed, Status};
use comet_proto::ChatIndicator;

/// Fixed tab width — the canvas draws a 150×28 tab
/// (`docs/design/canvas/comet-window.dc.html:46`, claim B4).
pub(super) const SESSION_TAB_WIDTH: f32 = 150.0;
/// The status dot leading a tab, and the slot the Working spinner shares with
/// it — the same 5px the sidebar's chat rows use (`spaces.rs::status_rail`).
/// `Theme` names no dot diameter; the sidebar inlines this number too.
const TAB_DOT: f32 = 5.0;
/// The selected tab's close button (canvas line 49: an 18px box, radius 6,
/// `--subtle`, holding an 11px glyph).
const TAB_CLOSE: f32 = 18.0;
const TAB_CLOSE_GLYPH: f32 = 11.0;
/// Flex gap between tabs — part of the drop-index slot width.
const TAB_GAP: f32 = 4.0;
/// Width of the overflow edge fades. Wide enough that per-glyph fade steps
/// (title text fades glyph-by-glyph on glass) stay gentle.
const FADE_WIDTH: f32 = 36.0;

/// Drag-reorder state; `epoch` keys the 150ms slide animation restarts.
pub(super) struct TabDragState {
    from: usize,
    over: usize,
    epoch: usize,
    prev_over: usize,
}

/// The dragged-tab payload (gpui drag-and-drop), space-scoped.
struct TabDragPayload {
    space: String,
    from: usize,
    title: SharedString,
    dot: gpui::Hsla,
}

/// The floating tab rendered at the cursor while dragging — a copy of the tab,
/// so it leads with the same status dot (the ghost never animates: a spinner
/// under the cursor is motion about the drag, not about the run).
struct TabGhost {
    title: SharedString,
    dot: gpui::Hsla,
}

impl Render for TabGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        div()
            .w(px(SESSION_TAB_WIDTH))
            .h(px(28.0))
            .px(px(Theme::SPACE_SM))
            .flex()
            .items_center()
            .gap(px(6.0))
            .rounded(px(Theme::RADIUS_CHIP))
            .bg(theme.surface_raised)
            .border_1()
            .border_color(theme.border_strong)
            .text_size(px(Theme::TEXT_DENSE))
            .text_color(theme.text)
            .opacity(0.85)
            .child(
                div()
                    .size(px(TAB_DOT))
                    // round-ok: status dot
                    .rounded_full()
                    .flex_none()
                    .bg(self.dot),
            )
            .child(div().truncate().child(self.title.clone()))
    }
}

/// Resolve the visual tab order for a space: the manual (drag) order first —
/// skipping chats that no longer exist — then any new chats appended in
/// creation order. Pure.
pub(super) fn resolve_tab_order(created_order: &[String], manual: &[String]) -> Vec<String> {
    let mut out: Vec<String> = manual
        .iter()
        .filter(|id| created_order.contains(id))
        .cloned()
        .collect();
    for id in created_order {
        if !out.contains(id) {
            out.push(id.clone());
        }
    }
    out
}

/// The neighbor to select after closing `closed`: the next tab, else the
/// previous, else `None` (last tab → new-session canvas). Pure.
pub(super) fn next_after_close(order: &[String], closed: &str) -> Option<String> {
    let ix = order.iter().position(|id| id == closed)?;
    if order.len() <= 1 {
        return None;
    }
    Some(if ix + 1 < order.len() {
        order[ix + 1].clone()
    } else {
        order[ix - 1].clone()
    })
}

impl Shell {
    /// The space's tabs in VISUAL order (manual drag order over creation
    /// order). Also the order of the sidebar's disclosed session rows
    /// (gh#124) — the two surfaces must agree.
    pub(super) fn tab_ids(&self, space_id: &str, cx: &App) -> Vec<String> {
        let created: Vec<String> = self
            .state
            .read(cx)
            .chats_in_space(space_id)
            .iter()
            .map(|c| c.id.clone())
            .collect();
        match self.settings.tab_order.get(space_id) {
            Some(manual) => resolve_tab_order(&created, manual),
            None => created,
        }
    }

    /// Close a tab = archive the session. Selection moves to a neighbor; the
    /// last tab lands on the new-session canvas.
    pub(super) fn close_session_tab(&mut self, chat_id: String, cx: &mut Context<Self>) {
        let (selected, order) = {
            let space = self.state.read(cx).selected_space.clone();
            let order = space
                .as_deref()
                .map(|space| self.tab_ids(space, cx))
                .unwrap_or_default();
            (self.state.read(cx).selected_chat.clone(), order)
        };
        if selected.as_deref() == Some(chat_id.as_str()) {
            let next = next_after_close(&order, &chat_id);
            self.state.update(cx, |s, cx| s.select_chat(next, cx));
        }
        self.archive_chat(chat_id, cx);
    }

    /// Track the drop slot while a tab is dragged over the strip (150ms sibling
    /// slides restart per committed `over` change — terminal-panel idiom).
    fn update_tab_drag_over(&mut self, from: usize, over: usize, cx: &mut Context<Self>) {
        match &mut self.tab_drag {
            Some(drag) if drag.from == from => {
                if drag.over != over {
                    drag.prev_over = drag.over;
                    drag.over = over;
                    drag.epoch += 1;
                    cx.notify();
                }
            }
            _ => {
                self.tab_drag = Some(TabDragState {
                    from,
                    over,
                    epoch: 0,
                    prev_over: from,
                });
                cx.notify();
            }
        }
    }

    /// Commit a drag: persist the new visual order for the space (device-local).
    fn commit_tab_reorder(&mut self, space: &str, from: usize, to: usize, cx: &mut Context<Self>) {
        let mut order = self.tab_ids(space, cx);
        if from < order.len() {
            reorder_tabs(&mut order, from, to);
            self.settings.tab_order.insert(space.to_string(), order);
            self.schedule_save(cx);
        }
        self.tab_drag = None;
        cx.notify();
    }

    /// The tab strip: [scrollable tabs (edge fades)][+][drag spacer][toggle-changes].
    pub(super) fn render_session_tab_strip(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        // A drag that ended off-strip (no drop event) must not strand the
        // sibling slide offsets.
        if self.tab_drag.is_some() && !cx.has_active_drag() {
            self.tab_drag = None;
        }
        let space_id = self.state.read(cx).selected_space.clone();
        let order: Vec<String> = space_id
            .as_deref()
            .map(|space| self.tab_ids(space, cx))
            .unwrap_or_default();
        // No harness mark on a tab: the canvas leads with the status dot and
        // nothing else (claim B6). The harness still identifies the run in the
        // sidebar's chat rows.
        let tabs: Vec<(String, SharedString, ChatIndicator)> = {
            let state = self.state.read(cx);
            order
                .iter()
                .filter_map(|id| {
                    let chat = state.chats.iter().find(|c| c.id == *id)?;
                    Some((
                        chat.id.clone(),
                        SharedString::from(transcript::single_line(
                            &chat.title.clone().unwrap_or_else(|| "New session".into()),
                        )),
                        state.display_status_for(chat, now),
                    ))
                })
                .collect()
        };
        // The review's own tab, and with it the whole route's selection: on
        // this route the REVIEW is what is selected, so the authoring session —
        // which is genuinely selected in state, because the column beside the
        // card is it — must not also read as the selected tab. Two lit tabs
        // would be two answers to "where am I".
        let review_tab: Option<SharedString> = match &self.route {
            Route::Review { task_id, .. } => Some(match &self.review {
                Some(panel) => panel.read(cx).tab_title(),
                // The route outliving its panel is the same fallback the route
                // itself takes (`render_review_route`); name it anyway.
                None => SharedString::from(crate::review::review_tab_title(None, task_id)),
            }),
            _ => None,
        };
        let on_review = review_tab.is_some();
        // Slots the chats are offset by — see the module header.
        let lead = usize::from(on_review);
        let selected = if on_review {
            None
        } else {
            self.state.read(cx).selected_chat.clone()
        };
        // Keep the selected tab visible: on selection change, scroll it into
        // view (minimal movement — a new session's tab materializes at the far
        // right of an overflowing strip and would otherwise be stranded
        // off-screen).
        match selected.as_deref() {
            Some(sel) if self.tabs_scrolled_to.as_deref() != Some(sel) => {
                if let Some(ix) = order.iter().position(|id| id == sel) {
                    self.tabs_scroll.scroll_to_item(ix + lead);
                }
                self.tabs_scrolled_to = Some(sel.to_string());
            }
            Some(_) => {}
            None => self.tabs_scrolled_to = None,
        }
        let has_space = space_id.is_some();
        let git = self.space_git_detected(cx);
        let hovered = self.tab_hover.clone();
        // The `+` carries the active wash only where the new-session canvas is
        // actually what is showing — `selected` is None on the review route
        // too, and a lit `+` there would claim the wrong destination.
        let on_canvas = selected.is_none() && !on_review;
        // No sessions yet → the canvas already shows; a `+` would be redundant.
        let has_tabs = !tabs.is_empty();
        let count = tabs.len();
        let drag = self
            .tab_drag
            .as_ref()
            .map(|d| (d.from, d.over, d.epoch, d.prev_over));

        let tab_elements: Vec<AnyElement> = tabs
            .into_iter()
            .enumerate()
            .map(|(ix, (id, title, status))| {
                let is_selected = selected.as_deref() == Some(id.as_str());
                let is_hovered = hovered.as_deref() == Some(id.as_str());
                // Hover state lives in Shell, so the wash snaps off it — gpui
                // allows only one `on_hover` per element, and the state
                // listener wins.
                // Three tones, three states: the selected tab reads as a
                // title, hover lifts to body, the rest sit at label weight.
                // The surface comes from the app's row helper — a tab is a
                // list row that happens to be horizontal (gh#175).
                let paint = theme.row(Bed::Shell, is_selected);
                let (text_color, bg) = if is_selected {
                    (
                        paint.text,
                        if is_hovered {
                            paint.hovered
                        } else {
                            paint.rest
                        },
                    )
                } else if is_hovered {
                    (theme.text_muted, paint.hovered)
                } else {
                    (theme.text_subtle, paint.rest)
                };
                let select_id = id.clone();
                let close_id = id.clone();
                let middle_id = id.clone();
                let hover_id = id.clone();
                let drag_space = space_id.clone().unwrap_or_default();
                // The dot LEADS the tab (claim B6, canvas line 47) and is the
                // only status colour on it — the title stays a text tone. It is
                // the sidebar's chat-row rail exactly: a 5px slot, and Working
                // animates in it (the miniaturized gradient spinner) instead of
                // sitting still.
                let dot = spaces::status_dot_color(status, &theme);
                let leading: AnyElement = if status == ChatIndicator::Working {
                    div()
                        .w(px(TAB_DOT))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(loaders::mini_gradient_spinner(2.0, cx.entity_id(), cx))
                        .into_any_element()
                } else {
                    div()
                        .size(px(TAB_DOT))
                        // round-ok: status dot
                        .rounded_full()
                        .flex_none()
                        .bg(dot)
                        .into_any_element()
                };
                // Only the SELECTED tab carries a close button (claim B7), and
                // it carries it at rest — not on hover. An unselected tab is
                // closed by selecting it first, or by middle-click.
                // NB: no `.occlude()` on the close button — the TAB already
                // occludes (for the titlebar drag region), and an occluding
                // child would block the tab's own hover hit-test: a flicker
                // loop (user-reported). `stop_propagation` on click is enough.
                let trailing: Option<AnyElement> = is_selected.then(|| {
                    div()
                        .id(SharedString::from(format!("session-tab-close-{id}")))
                        .size(px(TAB_CLOSE))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(Theme::RADIUS_CHIP))
                        .hover(|s| s.bg(theme.wash(0.14)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.close_session_tab(close_id.clone(), cx);
                        }))
                        .child(
                            icon(icons::CLOSE)
                                .size(px(TAB_CLOSE_GLYPH))
                                .text_color(theme.text_subtle),
                        )
                        .into_any_element()
                });
                let tab_el = div()
                    .id(SharedString::from(format!("session-tab-{id}")))
                    .w(px(SESSION_TAB_WIDTH))
                    .h(px(28.0))
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .pl(px(8.0))
                    .pr(px(4.0))
                    .rounded(px(Theme::RADIUS_ROW))
                    .text_size(px(Theme::TEXT_DENSE))
                    .text_color(text_color)
                    .bg(bg)
                    .shadow(paint.ring.clone())
                    .cursor_pointer()
                    // Tabs sit inside the titlebar drag strip — carve them out.
                    // NOT `.occlude()`: a BlockMouse hitbox ends the hit test,
                    // so the scroll container behind the tabs never saw wheel
                    // events and an overflowing strip could not be scrolled
                    // (tabs tile the whole region). ExceptScroll keeps the
                    // drag-region carve-out and lets the strip scroll.
                    .block_mouse_except_scroll()
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                    // Track hover in Shell state: the tab's three tones are
                    // picked in Rust (selected / hovered / at rest), so the
                    // hover has to be a value this render can read.
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if *hovered {
                            this.tab_hover = Some(hover_id.clone());
                        } else if this.tab_hover.as_deref() == Some(hover_id.as_str()) {
                            this.tab_hover = None;
                        }
                        cx.notify();
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        // Selecting another tab is one of the three ways out of
                        // a review (gh#311) — the same one every other surface
                        // in this app has.
                        if matches!(this.route, Route::Review { .. }) {
                            this.leave_review_for(select_id.clone(), cx);
                            return;
                        }
                        this.state
                            .update(cx, |s, cx| s.select_chat(Some(select_id.clone()), cx));
                    }))
                    // Middle-click closes (terminal-tab parity).
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.close_session_tab(middle_id.clone(), cx);
                        }),
                    )
                    .on_drag(
                        TabDragPayload {
                            space: drag_space,
                            from: ix,
                            title: title.clone(),
                            dot,
                        },
                        |payload, _point, _, cx| {
                            let title = payload.title.clone();
                            let dot = payload.dot;
                            cx.stop_propagation();
                            cx.new(|_| TabGhost { title, dot })
                        },
                    )
                    .child(leading)
                    .child(div().flex_1().min_w_0().truncate().child(title))
                    .children(trailing);

                // Sliding transform while a sibling is dragged over: animate
                // 150ms between committed offsets (terminal-panel idiom).
                match drag {
                    Some((from, over, epoch, prev_over)) if ix != from => {
                        let slot = SESSION_TAB_WIDTH + TAB_GAP;
                        let target = slide_offset(ix, from, over) * slot;
                        let start = slide_offset(ix, from, prev_over) * slot;
                        div()
                            .relative()
                            .child(tab_el.with_animation(
                                SharedString::from(format!("session-tab-slide-{id}-{epoch}")),
                                TAB_SLIDE.animation(),
                                move |el, t| el.left(px(motion::lerp(start, target, t))),
                            ))
                            .into_any_element()
                    }
                    // The dragged tab is represented by the cursor ghost; its
                    // flow slot renders as an INVISIBLE spacer. A dimmed tab
                    // here overlapped whatever sibling slid into the vacated
                    // slot (slide_offset moves one tab exactly there —
                    // user-reported double-exposure).
                    Some((from, ..)) if ix == from => div()
                        .w(px(SESSION_TAB_WIDTH))
                        .h(px(28.0))
                        .flex_none()
                        .into_any_element(),
                    _ => tab_el.into_any_element(),
                }
            })
            .collect();

        // The review's tab (gh#311; canvas
        // `comet-review-window.dc.html:44`): the strip's own measures, and the
        // selected paint — on this route it IS the selection. Two departures
        // from a chat tab, both forced by what a review is: the dot wears the
        // `--review` hue rather than a status, because a review is not a run
        // with a status; and the tab is not draggable, because drag order is
        // persisted per space and a review belongs to no space.
        let review_tab_el: Option<AnyElement> = review_tab.map(|title| {
            let paint = theme.row(Bed::Shell, true);
            div()
                .id("review-tab")
                .w(px(SESSION_TAB_WIDTH))
                .h(px(28.0))
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .pl(px(8.0))
                .pr(px(4.0))
                .rounded(px(Theme::RADIUS_ROW))
                .text_size(px(Theme::TEXT_DENSE))
                .text_color(paint.text)
                .bg(paint.rest)
                .hover(|s| s.bg(paint.hovered))
                .shadow(paint.ring.clone())
                // Same carve-out as a chat tab: out of the titlebar drag
                // region, but not out of the strip's own wheel scrolling.
                .block_mouse_except_scroll()
                .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                // Middle-click closes, as it does on a chat tab.
                .on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.close_review(cx);
                    }),
                )
                .child(
                    div()
                        .size(px(TAB_DOT))
                        // round-ok: status dot
                        .rounded_full()
                        .flex_none()
                        .bg(theme.status(Status::Review)),
                )
                .child(div().flex_1().min_w_0().truncate().child(title))
                .child(
                    div()
                        .id("review-tab-close")
                        .size(px(TAB_CLOSE))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(Theme::RADIUS_CHIP))
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.wash(0.14)))
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.close_review(cx);
                        }))
                        .child(
                            icon(icons::CLOSE)
                                .size(px(TAB_CLOSE_GLYPH))
                                .text_color(theme.text_subtle),
                        ),
                )
                .into_any_element()
        });

        // `+` invokes the same typed command as File → New Session and ⌘T.
        let new_tab = div()
            .id("session-tab-new")
            .size(px(28.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(Theme::RADIUS_CHIP))
            .cursor_pointer()
            .list_row(
                &theme,
                Bed::Shell,
                on_canvas && has_space,
                "session-tab-new",
            )
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
            .on_click(cx.listener(|this, _, _, cx| {
                cx.stop_propagation();
                this.new_session(None, cx);
            }))
            .child(icon(icons::PLUS).size(px(16.0)).text_color(theme.text_muted));

        // Overflow: the tab region scrolls horizontally; edge fades appear on
        // whichever side has hidden tabs (offset from the LAST frame — a
        // one-frame lag is invisible). On GLASS the fades are an EdgeFade
        // scope (per-glyph gradient); painted overlays only exist on opaque
        // platforms, in the SHELL surface tone the strip now sits on.
        let scrolled = -f32::from(self.tabs_scroll.offset().x);
        let max_scroll = f32::from(self.tabs_scroll.max_offset().x);
        let fade_left = scrolled > 1.0;
        let fade_right = scrolled < max_scroll - 1.0;
        let glass = Theme::GLASS_ALPHA < 1.0;
        let bar_bg = theme.surface;
        let drag_move_space = space_id.clone().unwrap_or_default();
        let drop_space = space_id.clone().unwrap_or_default();
        let scroll_for_drag = self.tabs_scroll.clone();
        let tab_region = div()
            .relative()
            .min_w_0()
            .child(
                div()
                    .id("session-tabs-scroll")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(TAB_GAP))
                    .min_w_0()
                    .overflow_x_scroll()
                    .track_scroll(&self.tabs_scroll)
                    .on_drag_move::<TabDragPayload>(cx.listener(
                        move |this, event: &gpui::DragMoveEvent<TabDragPayload>, _, cx| {
                            let payload = event.drag(cx);
                            if payload.space != drag_move_space {
                                return;
                            }
                            let from = payload.from;
                            // Drop math runs in CONTENT coordinates: viewport-
                            // relative x plus the scrolled-off width — and past
                            // the lead review tab, which is in this container
                            // but not in the chat order the indices address.
                            let rel_x = f32::from(event.event.position.x)
                                - f32::from(event.bounds.left())
                                + -f32::from(scroll_for_drag.offset().x)
                                - lead as f32 * (SESSION_TAB_WIDTH + TAB_GAP);
                            let over = drop_index(rel_x, SESSION_TAB_WIDTH + TAB_GAP, count);
                            this.update_tab_drag_over(from, over, cx);
                        },
                    ))
                    .on_drop::<TabDragPayload>(cx.listener(
                        move |this, payload: &TabDragPayload, _, cx| {
                            if payload.space != drop_space {
                                this.tab_drag = None;
                                cx.notify();
                                return;
                            }
                            let to = this
                                .tab_drag
                                .as_ref()
                                .map(|d| d.over)
                                .unwrap_or(payload.from);
                            let space = drop_space.clone();
                            this.commit_tab_reorder(&space, payload.from, to, cx);
                        },
                    ))
                    .children(review_tab_el)
                    .children(tab_elements),
            )
            .when(fade_left && !glass, |el| {
                el.child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(px(FADE_WIDTH))
                        .bg(gpui::linear_gradient(
                            90.0,
                            gpui::linear_color_stop(bar_bg, 0.0),
                            gpui::linear_color_stop(bar_bg.opacity(0.0), 1.0),
                        )),
                )
            })
            .when(fade_right && !glass, |el| {
                el.child(
                    div()
                        .absolute()
                        .right_0()
                        .top_0()
                        .bottom_0()
                        .w(px(FADE_WIDTH))
                        .bg(gpui::linear_gradient(
                            270.0,
                            gpui::linear_color_stop(bar_bg, 0.0),
                            gpui::linear_color_stop(bar_bg.opacity(0.0), 1.0),
                        )),
                )
            });
        let tab_region: AnyElement = if glass {
            crate::edge_fade::edge_faded(FADE_WIDTH, false, false, tab_region)
                .fade_left(fade_left)
                .fade_right(fade_right)
                .into_any_element()
        } else {
            tab_region.into_any_element()
        };

        // The strip starts right after the control cluster and STAYS there
        // (claim B3: x=172), whatever the sidebar does. It is titlebar
        // furniture, not a header for the card below it: the canvas runs the
        // first tab straight across the sidebar divider at x=256, with the
        // divider (claim A3) passing full height behind it.
        let tabs_left = self.title_bar_content_start();
        // The board toggle (§gh#70): a sibling of the changes
        // toggle, showing an active wash while the board dock is open. Not
        // gated on git — the board is a global queue, not a checkout view.
        let board_active = self.board_open;
        let board_button = div()
            .id("toggle-board")
            .size(px(28.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(Theme::RADIUS_CHIP))
            .cursor_pointer()
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
            .bg(motion::hover_blend(
                "toggle-board",
                theme.wash(if board_active { 0.14 } else { 0.0 }),
                theme.wash(0.2),
            ))
            .on_hover(motion::hover_listener("toggle-board"))
            // The route-aware toggle, which is what `mod-shift-b` runs too: the
            // strip is drawn on the review route now, where this gesture means
            // "leave the review and go back to the queue it came from".
            .on_click(cx.listener(|this, _, window, cx| {
                cx.stop_propagation();
                this.toggle_board_from_route(window, cx);
            }))
            .child(
                icon(icons::CHECKLIST)
                    .size(px(16.0))
                    .text_color(theme.text_muted),
            )
            .into_any_element();
        let inner = div()
            .size_full()
            .flex()
            .items_center()
            .pt(px(Theme::TITLEBAR_TOP_PAD))
            .gap(px(6.0))
            .pl(px(tabs_left))
            .pr(px(Theme::SPACE_LG))
            .child(tab_region)
            .when(has_space && has_tabs, |el| el.child(new_tab))
            .child(div().flex_1())
            // Stable location: the toggle shows whether the pane is open or
            // not (the pane's own header is gone).
            .child(board_button)
            // The changes pane is chat-route chrome (its action is gated the
            // same way): on the review route that slot already holds the
            // authoring session, so the toggle would toggle nothing.
            .when(git && !on_review, |el| {
                el.child(header_icon_button(
                    "toggle-changes",
                    icons::SIDEBAR_MINIMALISTIC,
                    &theme,
                    cx.listener(|this, _, _, cx| this.toggle_right_pane(cx)),
                ))
            });

        // The unified window titlebar: full-width on the glass shell, ABOVE
        // the inset card. No bottom border — the card's own hairline is the
        // separation; the glass gutter shows between.
        let bar = div().h(px(Theme::TITLEBAR_HEIGHT)).flex_none().child(inner);
        self.titlebar_drag_region("chat-tabs-titlebar", bar, cx)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{next_after_close, resolve_tab_order};

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn close_selects_next_then_previous_then_canvas() {
        let order = ids(&["a", "b", "c"]);
        assert_eq!(next_after_close(&order, "a").as_deref(), Some("b"));
        assert_eq!(next_after_close(&order, "b").as_deref(), Some("c"));
        // Last tab: fall back to the previous one.
        assert_eq!(next_after_close(&order, "c").as_deref(), Some("b"));
        // Only tab: canvas.
        assert_eq!(next_after_close(&ids(&["solo"]), "solo"), None);
        // Unknown id: no opinion.
        assert_eq!(next_after_close(&order, "zz"), None);
    }

    #[test]
    fn manual_order_wins_and_new_chats_append() {
        let created = ids(&["a", "b", "c", "d"]);
        // Manual order covers some chats; "gone" no longer exists.
        let manual = ids(&["c", "gone", "a"]);
        assert_eq!(
            resolve_tab_order(&created, &manual),
            ids(&["c", "a", "b", "d"])
        );
        // No manual order → creation order.
        assert_eq!(resolve_tab_order(&created, &[]), created);
        // Manual covers everything → manual order verbatim.
        assert_eq!(
            resolve_tab_order(&ids(&["a", "b"]), &ids(&["b", "a"])),
            ids(&["b", "a"])
        );
    }
}
