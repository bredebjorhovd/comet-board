//! Settings → Board routing (gh#75): the board host's `routing.toml`, what is
//! wrong with it, and the three changes people actually go to a shell for —
//! adding a repo, pointing a route at a different agent account, adjusting a
//! cap.
//!
//! The file lives on whichever device hosts the board, normally the always-on
//! box, which is exactly why this page exists: `ReadBoardConfig` and
//! `WriteBoardConfig` are relay-forwardable, so a teammate with no ssh account
//! on that box can still route a new repo. Every write goes through the board
//! crate's validating writer — it has to parse, it has to validate, and the
//! previous contents land in `routing.toml.bak` — so the worst this page can do
//! is refuse.
//!
//! ## Finding the host
//!
//! Same contract the board panel sweeps on (`crate::board`, gh#55): the engine
//! refuses the board methods outright when it hosts no board, so a candidate
//! that errors has answered "not me". This walks
//! [`comet_proto::view::board::host_candidates`] until one answers, which needs
//! no configuration and no coupling to whether the board panel has ever been
//! opened.

use gpui::{
    AnyElement, Context, Entity, SharedString, Subscription, Task, Window, div, prelude::*, px,
};

use comet_board::adopt::Unadopted;
use comet_board::config::Route;
use comet_board::onboard::{Candidate, Onboarded};
use comet_board::routes::{RoutingView, cap_summary, match_summary};
use comet_proto::view::board;
use comet_rpc::methods;
use serde::Deserialize;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

/// What both config methods answer with.
#[derive(Debug, Clone, Deserialize)]
struct BoardConfig {
    routing: RoutingView,
    #[serde(default)]
    unadopted: Vec<Unadopted>,
}

/// One route key this page can set, and what to call it.
///
/// A short list on purpose: these are the edits somebody opens a terminal for.
/// Everything else — a prompt, a match, the order of the routes — is still a
/// file edit, and `comet-board routes edit` is the surface for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteField {
    Account,
    MaxDuration,
    Runtime,
}

impl RouteField {
    /// The `routing.toml` key. What goes on the wire.
    pub fn key(self) -> &'static str {
        match self {
            RouteField::Account => "account",
            RouteField::MaxDuration => "max_duration",
            RouteField::Runtime => "runtime",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            RouteField::Account => "Agent account",
            RouteField::MaxDuration => "Attempt cap",
            RouteField::Runtime => "Runtime",
        }
    }

    /// What an empty field means, said out loud in the dialog. Clearing these
    /// is a real choice — it falls the route back to `[defaults]` — and a
    /// person needs to know that before they clear one.
    pub fn empty_hint(self) -> &'static str {
        match self {
            RouteField::Account => "Empty: the device's own CLI login.",
            RouteField::MaxDuration => "Empty: the [defaults] cap. `off` for none.",
            RouteField::Runtime => "Required — a route with no runtime cannot dispatch.",
        }
    }

    pub fn current(self, route: &Route) -> Option<String> {
        match self {
            RouteField::Account => route.account.clone(),
            RouteField::MaxDuration => route.max_duration.clone(),
            RouteField::Runtime => Some(route.runtime.clone()),
        }
    }
}

struct FieldDialog {
    route: usize,
    field: RouteField,
    route_name: String,
    input: Entity<ComposerInput>,
    _events: Subscription,
}

/// The "Onboard a repo…" panel (gh#97).
///
/// The list is the board App's *grant*, fetched from the host — not the
/// operator's repos, and not this device's folders. That is the honest set:
/// every repo in it is one the box can clone and the sync loop can poll, and a
/// repo missing from it needs somebody to install the App, which is not a thing
/// this page could do for them.
///
/// The free-text field is not a fallback for a broken list, it is the other half
/// of the surface: a board authenticating with `GITHUB_TOKEN` has no
/// installations to enumerate at all, and the picker would otherwise be empty
/// for it forever.
struct OnboardFlow {
    /// `None` while the list is in flight.
    repos: Option<Vec<Candidate>>,
    /// Why there is no list. Not an error on the page — a board on a personal
    /// access token is a supported board, and it just types the repo instead.
    list_note: Option<SharedString>,
    slug: Entity<ComposerInput>,
    dir: Entity<ComposerInput>,
    _events: Subscription,
}

pub struct RoutingPage {
    state: Entity<AppState>,
    /// Which device's board — `None` is this one. Resolved by the same sweep
    /// the board panel uses.
    host: Option<String>,
    config: Option<BoardConfig>,
    dialog: Option<FieldDialog>,
    onboard: Option<OnboardFlow>,
    /// What the last onboard did, kept on screen until the panel is closed —
    /// it is the only record of a clone that happened on a machine the reader
    /// may have no other view of.
    onboarded: Option<Onboarded>,
    error: Option<SharedString>,
    /// A write is in flight; the page's buttons go quiet rather than queueing a
    /// second edit against a file the first one is still rewriting.
    busy: bool,
    /// What the busy state is waiting on. An onboard is a `git clone` on another
    /// machine — seconds to minutes — and a page that only greys out says
    /// nothing about whether it is working or wedged.
    busy_note: Option<SharedString>,
    loaded: bool,
    task: Option<Task<()>>,
    /// Onboarding runs on its own slot: dropping a gpui `Task` cancels it, and a
    /// config reload landing mid-clone must not look like a cancelled clone.
    onboard_task: Option<Task<()>>,
    _observe: Subscription,
}

impl RoutingPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let mut page = Self {
            state,
            host: None,
            config: None,
            dialog: None,
            onboard: None,
            onboarded: None,
            error: None,
            busy: false,
            busy_note: None,
            loaded: false,
            task: None,
            onboard_task: None,
            _observe: observe,
        };
        page.reload(cx);
        page
    }

    fn host_params(&self, value: serde_json::Value) -> serde_json::Value {
        let mut value = value;
        if let (Some(host), Some(object)) = (self.host.as_deref(), value.as_object_mut()) {
            object.insert("targetDeviceId".into(), serde_json::json!(host));
        }
        value
    }

    /// The host's display name, for the header line.
    fn host_label(&self, cx: &gpui::App) -> SharedString {
        let Some(host) = self.host.as_deref() else {
            return "this device".into();
        };
        self.state
            .read(cx)
            .devices
            .iter()
            .find(|d| d.id == host)
            .map(|d| SharedString::from(d.name.clone()))
            .unwrap_or_else(|| SharedString::from(host.to_string()))
    }

    /// Read the config, sweeping for the host that has one.
    ///
    /// A candidate that errors has answered "I host no board" — that is the
    /// engine's contract for every board method — so the sweep moves on. When
    /// nobody answers, the last error is what the page shows: "board
    /// unavailable" from every device is a true and useful thing to read.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine not connected".into());
            return;
        };
        let (devices, local) = {
            let state = self.state.read(cx);
            (state.devices.clone(), state.local_device_id.clone())
        };
        let candidates = board::host_candidates(&devices, local.as_deref());
        self.error = None;
        self.task = Some(cx.spawn(async move |this, cx| {
            let mut last: Option<String> = None;
            for candidate in candidates {
                let mut params = serde_json::json!({});
                if let (Some(host), Some(object)) = (candidate.as_deref(), params.as_object_mut()) {
                    object.insert("targetDeviceId".into(), serde_json::json!(host));
                }
                match engine
                    .client()
                    .call(methods::READ_BOARD_CONFIG, params)
                    .await
                {
                    Ok(value) => {
                        let parsed = serde_json::from_value::<BoardConfig>(value);
                        let updated = this.update(cx, |page, cx| {
                            page.loaded = true;
                            match parsed {
                                Ok(config) => {
                                    page.host = candidate;
                                    page.config = Some(config);
                                }
                                Err(err) => {
                                    page.error = Some(format!("Unreadable config: {err}").into())
                                }
                            }
                            cx.notify();
                        });
                        let _ = updated;
                        return;
                    }
                    Err(err) => last = Some(err.to_string()),
                }
            }
            this.update(cx, |page, cx| {
                page.loaded = true;
                page.error = Some(
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
    }

    /// Send one write and replace the page's config with its reply — which is a
    /// fresh read, so what is on screen afterwards is the file as it now stands.
    fn write(&mut self, op: serde_json::Value, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        self.busy = true;
        self.error = None;
        let params = self.host_params(op);
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::WRITE_BOARD_CONFIG, params)
                .await;
            this.update(cx, |page, cx| {
                page.busy = false;
                match result {
                    Ok(value) => match serde_json::from_value::<BoardConfig>(value) {
                        Ok(config) => page.config = Some(config),
                        Err(err) => page.error = Some(format!("Unreadable reply: {err}").into()),
                    },
                    // The refusals are the interesting ones and they are
                    // written to be read: "would not have validated: route 2
                    // has runtime `codx`, which is not a comet harness".
                    Err(err) => page.error = Some(format!("{err}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn open_dialog(
        &mut self,
        route: usize,
        field: RouteField,
        route_name: String,
        current: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| ComposerInput::new(field.label(), cx));
        if let Some(current) = current {
            input.update(cx, |input, cx| input.set_text(current, cx));
        }
        let events = cx.subscribe(&input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_dialog(cx);
            }
        });
        self.dialog = Some(FieldDialog {
            route,
            field,
            route_name,
            input,
            _events: events,
        });
        cx.notify();
    }

    fn submit_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.dialog.take() else {
            return;
        };
        let text = dialog.input.read(cx).text().trim().to_string();
        // An emptied field is a removal, not an empty string: `account = ""`
        // would name a slot that does not exist, and every dispatch on the
        // route would then fail. Absent falls back to `[defaults]`, which is
        // what clearing it means.
        let value = (!text.is_empty()).then_some(text);
        self.write(
            serde_json::json!({
                "op": "route",
                "route": dialog.route,
                "key": dialog.field.key(),
                "value": value,
            }),
            cx,
        );
    }

    fn adopt(&mut self, slug: String, cx: &mut Context<Self>) {
        self.write(serde_json::json!({ "op": "adopt", "slug": slug }), cx);
    }

    // ---- onboarding a repo the board has never seen (gh#97) ----

    /// Open the panel and ask the host what its App can see.
    fn open_onboard(&mut self, cx: &mut Context<Self>) {
        let slug = cx.new(|cx| ComposerInput::new("owner/repo", cx));
        let dir = cx.new(|cx| ComposerInput::new("Folder on the board's device (optional)", cx));
        let events = cx.subscribe(&slug, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_onboard(None, cx);
            }
        });
        self.onboard = Some(OnboardFlow {
            repos: None,
            list_note: None,
            slug,
            dir,
            _events: events,
        });
        self.onboarded = None;
        self.error = None;
        cx.notify();

        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.host_params(serde_json::json!({}));
        self.onboard_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::LIST_APP_REPOS, params).await;
            this.update(cx, |page, cx| {
                let Some(flow) = page.onboard.as_mut() else {
                    return;
                };
                match result {
                    Ok(value) => match serde_json::from_value::<Vec<Candidate>>(value) {
                        Ok(repos) => flow.repos = Some(repos),
                        Err(err) => {
                            flow.repos = Some(Vec::new());
                            flow.list_note = Some(format!("Unreadable reply: {err}").into());
                        }
                    },
                    // Not the page's error strip: the commonest cause is a board
                    // on a personal access token, which has no installations to
                    // list and is not broken.
                    Err(err) => {
                        flow.repos = Some(Vec::new());
                        flow.list_note = Some(format!("{err}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Onboard `slug` — from a picker row, or from the typed field when `None`.
    fn submit_onboard(&mut self, slug: Option<String>, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(flow) = self.onboard.as_ref() else {
            return;
        };
        let typed = flow.slug.read(cx).text().trim().to_string();
        let dir = flow.dir.read(cx).text().trim().to_string();
        let raw = slug.unwrap_or(typed);
        // Parsed here rather than on the box: a typo should cost a message, not
        // a round trip that comes back with the same news.
        let slug = match comet_board::onboard::parse_slug(&raw) {
            Ok(slug) => slug,
            Err(err) => {
                self.error = Some(format!("{err}").into());
                cx.notify();
                return;
            }
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine not connected".into());
            cx.notify();
            return;
        };

        let mut params = serde_json::json!({ "slug": slug });
        if let (false, Some(object)) = (dir.is_empty(), params.as_object_mut()) {
            object.insert("dir".into(), serde_json::json!(dir));
        }
        let params = self.host_params(params);
        self.busy = true;
        self.busy_note = Some(format!("Cloning {slug} on the board's device…").into());
        self.error = None;
        self.onboarded = None;
        cx.notify();

        self.onboard_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::ONBOARD_REPO, params).await;
            this.update(cx, |page, cx| {
                page.busy = false;
                page.busy_note = None;
                match result {
                    Ok(value) => match serde_json::from_value::<Onboarded>(value) {
                        Ok(done) => {
                            page.onboarded = Some(done);
                            // The routes list is now stale by exactly the route
                            // this wrote; re-read rather than patch it in.
                            page.reload(cx);
                        }
                        Err(err) => page.error = Some(format!("Unreadable reply: {err}").into()),
                    },
                    // These refusals are written to be read: "cannot onboard
                    // acme/thing: the board's GitHub App (id 123) cannot see it
                    // … install it on acme".
                    Err(err) => page.error = Some(format!("{err}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn ignore(&mut self, slug: String, cx: &mut Context<Self>) {
        self.write(serde_json::json!({ "op": "ignore", "slug": slug }), cx);
    }
}

impl Render for RoutingPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let host = self.host_label(cx);
        let config = self.config.clone();
        let busy = self.busy;

        let mut column = widgets::page_column()
            .child(widgets::page_header(
                &theme,
                "Board routing",
                config
                    .as_ref()
                    .and_then(|c| c.routing.config.as_ref())
                    .map(|c| c.routes.len()),
            ))
            .child(widgets::page_subtitle(
                &theme,
                match &config {
                    Some(c) => format!("{} on {host}", c.routing.path),
                    None if self.loaded => "No board found".to_string(),
                    None => "Reading…".to_string(),
                },
            ));

        if let Some(error) = self.error.clone() {
            column = column.child(
                widgets::error_strip(error)
                    .id("routing-error")
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.error = None;
                        cx.notify();
                    })),
            );
        }

        // Before the routes: this is the thing somebody came here to do that a
        // list of existing routes cannot help them with.
        column = column.child(self.render_onboard(&theme, busy, cx));

        if let Some(config) = &config {
            // What is wrong with the file, before what is in it: this is the
            // config the board is NOT running on, and the routes below it are
            // then a description of a file rather than of the board.
            for (ix, problem) in config.routing.problems.iter().enumerate() {
                column = column
                    .child(widgets::warning_strip(problem.clone()).id(("routing-problem", ix)));
            }

            if let Some(parsed) = &config.routing.config {
                let default_cap = parsed.defaults.max_duration.clone();
                let mut card = widgets::section_card(&theme);
                if parsed.routes.is_empty() {
                    card = card.child(
                        widgets::card_row(&theme, true).child(
                            div()
                                .text_size(px(Theme::TEXT_DENSE))
                                .text_color(theme.text_muted)
                                .child(SharedString::from(
                                    "No routes — every row on the board reads `no route`.",
                                )),
                        ),
                    );
                }
                for (ix, route) in parsed.routes.iter().enumerate() {
                    card = card.child(self.render_route(&theme, ix, route, &default_cap, busy, cx));
                }
                column = column.child(card);
            }

            if !config.unadopted.is_empty() {
                column = column
                    .child(
                        div()
                            .mt(px(28.0))
                            .text_size(px(Theme::TEXT_BODY))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(SharedString::from("Not on the board yet")),
                    )
                    .child(widgets::page_subtitle(
                        &theme,
                        "Repos with a space on the board's device that nothing is polling or \
                         routing. Adding one writes both halves.",
                    ));
                let mut card = widgets::section_card(&theme);
                for (ix, repo) in config.unadopted.iter().enumerate() {
                    card = card.child(self.render_unadopted(&theme, ix, repo, busy, cx));
                }
                column = column.child(card);
            }
        }

        if let Some(dialog) = &self.dialog {
            column = column.child(self.render_dialog(&theme, dialog, cx));
        }

        div()
            .id("routing-page")
            .size_full()
            .overflow_y_scroll()
            .child(column)
    }
}

impl RoutingPage {
    fn render_route(
        &self,
        theme: &Theme,
        ix: usize,
        route: &Route,
        default_cap: &str,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name: SharedString = route.display_name().to_string().into();
        let match_: SharedString = match_summary(&route.match_).into();
        let meta: SharedString = format!(
            "{} · {} · {}",
            route.workspace,
            route.repo,
            cap_summary(route, default_cap)
        )
        .into();
        let account: SharedString = route
            .account
            .clone()
            .unwrap_or_else(|| "device login".into())
            .into();
        let runtime: SharedString = route.runtime.clone().into();

        let field_button = |field: RouteField, label: SharedString, route: &Route| {
            let current = field.current(route);
            let route_name = route.display_name().to_string();
            widgets::ghost_action(theme)
                .id(("route-field", ix * 8 + field.key().len()))
                .hover(widgets::ghost_hover(theme))
                .when(busy, |el| el.opacity(0.4))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.busy {
                        return;
                    }
                    this.open_dialog(ix, field, route_name.clone(), current.clone(), cx);
                }))
                .child(label)
        };

        widgets::card_row(theme, ix == 0)
            .id(("route", ix))
            .items_start()
            .child(widgets::row_tile(theme, crate::icons::GIT_BRANCH))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(widgets::row_title(theme, name))
                    .child(widgets::meta_line(
                        theme,
                        vec![
                            div().child(match_).into_any_element(),
                            div().min_w_0().truncate().child(meta).into_any_element(),
                        ],
                    ))
                    .child(
                        div()
                            .mt(px(6.0))
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap(px(4.0))
                            .child(field_button(
                                RouteField::Runtime,
                                format!("runtime: {runtime}").into(),
                                route,
                            ))
                            .child(field_button(
                                RouteField::Account,
                                format!("account: {account}").into(),
                                route,
                            ))
                            .child(field_button(
                                RouteField::MaxDuration,
                                format!("cap: {}", cap_summary(route, default_cap)).into(),
                                route,
                            )),
                    ),
            )
            .child(widgets::badge(theme, format!("{}", ix + 1)))
            .into_any_element()
    }

    /// The "Onboard a repo…" affordance (gh#97): closed it is one button, open
    /// it is the App's repo list plus a field for naming one directly.
    ///
    /// Distinct from "Not on the board yet" below, and the difference is the
    /// whole point of the ticket: that list is repos with a *checkout already on
    /// the box*, which somebody had to make by hand. This one starts from repos
    /// the box has never seen.
    fn render_onboard(&self, theme: &Theme, busy: bool, cx: &mut Context<Self>) -> AnyElement {
        let Some(flow) = &self.onboard else {
            return widgets::section_card(theme)
                .child(
                    widgets::card_row(theme, true)
                        .child(widgets::row_tile(theme, crate::icons::FOLDER))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(widgets::row_title(theme, "Onboard a repo…"))
                                .child(widgets::meta_line(
                                    theme,
                                    vec![
                                        div()
                                            .min_w_0()
                                            .child(SharedString::from(
                                                "Clone it on the board's device, give it a \
                                                 space, and route it — in one step",
                                            ))
                                            .into_any_element(),
                                    ],
                                )),
                        )
                        .child(
                            widgets::ghost_action(theme)
                                .id("onboard-open")
                                .hover(widgets::ghost_hover(theme))
                                .when(busy, |el| el.opacity(0.4))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if this.busy {
                                        return;
                                    }
                                    this.open_onboard(cx);
                                }))
                                .child(SharedString::from("Onboard")),
                        ),
                )
                .into_any_element();
        };

        let mut card = widgets::section_card(theme).child(
            widgets::card_row(theme, true)
                .flex_col()
                .items_start()
                .gap(px(8.0))
                .child(widgets::row_title(theme, "Onboard a repo"))
                .child(
                    div()
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(theme.text_subtle)
                        .child(SharedString::from(
                            "The repos the board's GitHub App can see. Onboarding clones on \
                             the board's device with the board's own credential — this \
                             machine needs neither a checkout nor a GitHub login.",
                        )),
                )
                .child(div().w_full().child(flow.slug.clone()))
                .child(div().w_full().child(flow.dir.clone()))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(6.0))
                        .child(
                            widgets::ghost_action(theme)
                                .id("onboard-submit")
                                .hover(widgets::ghost_hover(theme))
                                .when(busy, |el| el.opacity(0.4))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.submit_onboard(None, cx);
                                }))
                                .child(SharedString::from("Onboard")),
                        )
                        .child(
                            widgets::ghost_action(theme)
                                .id("onboard-close")
                                .hover(widgets::ghost_hover(theme))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.onboard = None;
                                    this.onboarded = None;
                                    cx.notify();
                                }))
                                .child(SharedString::from("Close")),
                        ),
                ),
        );

        if let Some(note) = self.busy_note.clone() {
            card = card.child(
                widgets::card_row(theme, false).child(
                    div()
                        .text_size(px(Theme::TEXT_DENSE))
                        .text_color(theme.text_muted)
                        .child(note),
                ),
            );
        }

        match &flow.repos {
            None => {
                card = card.child(
                    widgets::card_row(theme, false).child(
                        div()
                            .text_size(px(Theme::TEXT_DENSE))
                            .text_color(theme.text_muted)
                            .child(SharedString::from("Asking the board what its App can see…")),
                    ),
                );
            }
            Some(repos) => {
                if let Some(note) = flow.list_note.clone() {
                    card = card.child(
                        widgets::card_row(theme, false).child(
                            div()
                                .text_size(px(Theme::TEXT_DENSE))
                                .text_color(theme.text_muted)
                                .child(note),
                        ),
                    );
                }
                for (ix, repo) in repos.iter().enumerate() {
                    card = card.child(self.render_candidate(theme, ix, repo, busy, cx));
                }
            }
        }

        if let Some(done) = &self.onboarded {
            card = card.child(self.render_onboarded(theme, done));
        }
        card.into_any_element()
    }

    fn render_candidate(
        &self,
        theme: &Theme,
        ix: usize,
        repo: &Candidate,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let slug: SharedString = repo.slug.clone().into();
        let note: SharedString = repo.note().into();
        let on_board = repo.on_board();
        let pick = repo.slug.clone();
        widgets::card_row(theme, false)
            .id(("candidate", ix))
            .child(widgets::row_tile(theme, crate::icons::GIT_BRANCH))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(widgets::row_title(theme, slug))
                    .child(widgets::meta_line(
                        theme,
                        vec![div().min_w_0().truncate().child(note).into_any_element()],
                    )),
            )
            // A repo already polled and routed keeps its row — "is this one set
            // up?" is half of why the panel gets opened — but nothing to press.
            .when(on_board, |el| {
                el.child(widgets::badge_active(
                    theme,
                    SharedString::from("On the board"),
                ))
            })
            .when(!on_board, |el| {
                el.child(
                    widgets::ghost_action(theme)
                        .id(("onboard-pick", ix))
                        .hover(widgets::ghost_hover(theme))
                        .when(busy, |el| el.opacity(0.4))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.submit_onboard(Some(pick.clone()), cx);
                        }))
                        .child(SharedString::from("Onboard")),
                )
            })
            .into_any_element()
    }

    /// What the last onboard did, step by step — the only record of work that
    /// happened on a machine the reader may have no other view of.
    fn render_onboarded(&self, theme: &Theme, done: &Onboarded) -> AnyElement {
        let mut lines: Vec<String> = vec![
            format!("clone {} — {}", done.clone.as_str(), done.path),
            format!("space {} — {}", done.space.as_str(), done.space_name),
            match &done.adopted {
                None => "routing unchanged — already polled and routed".to_string(),
                Some(a) => format!(
                    "routing wrote{}{}",
                    if a.wrote_route { " a [[route]]" } else { "" },
                    if a.wrote_repo { " [github] repos" } else { "" },
                ),
            },
        ];
        if let Some(p) = &done.preview {
            lines.push(format!("{} to poll", p.count_phrase()));
        }
        lines.extend(done.notes());

        let mut row = widgets::card_row(theme, false)
            .flex_col()
            .items_start()
            .gap(px(4.0))
            .child(widgets::row_title(theme, done.slug.clone()));
        for line in lines {
            row = row.child(
                div()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(theme.text_subtle)
                    .child(SharedString::from(line)),
            );
        }
        row.into_any_element()
    }

    fn render_unadopted(
        &self,
        theme: &Theme,
        ix: usize,
        repo: &Unadopted,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let slug: SharedString = repo.slug.clone().into();
        let note: SharedString = format!("{}{}", repo.label, repo.missing.note()).into();
        let add_slug = repo.slug.clone();
        let ignore_slug = repo.slug.clone();
        widgets::card_row(theme, ix == 0)
            .id(("unadopted", ix))
            .child(widgets::row_tile(theme, crate::icons::FOLDER))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(widgets::row_title(theme, slug))
                    .child(widgets::meta_line(
                        theme,
                        vec![div().min_w_0().truncate().child(note).into_any_element()],
                    )),
            )
            .child(
                widgets::ghost_action(theme)
                    .id(("adopt", ix))
                    .hover(widgets::ghost_hover(theme))
                    .when(busy, |el| el.opacity(0.4))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.busy {
                            return;
                        }
                        this.adopt(add_slug.clone(), cx);
                    }))
                    .child(SharedString::from("Add")),
            )
            .child(
                widgets::ghost_action(theme)
                    .id(("ignore", ix))
                    .hover(widgets::ghost_hover(theme))
                    .when(busy, |el| el.opacity(0.4))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.busy {
                            return;
                        }
                        this.ignore(ignore_slug.clone(), cx);
                    }))
                    .child(SharedString::from("Ignore")),
            )
            .into_any_element()
    }

    fn render_dialog(
        &self,
        theme: &Theme,
        dialog: &FieldDialog,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        widgets::section_card(theme)
            .child(
                widgets::card_row(theme, true)
                    .flex_col()
                    .items_start()
                    .gap(px(8.0))
                    .child(widgets::row_title(
                        theme,
                        format!("{} — {}", dialog.field.label(), dialog.route_name),
                    ))
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(theme.text_subtle)
                            .child(SharedString::from(dialog.field.empty_hint())),
                    )
                    .child(div().w_full().child(dialog.input.clone()))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(6.0))
                            .child(
                                widgets::ghost_action(theme)
                                    .id("routing-dialog-save")
                                    .hover(widgets::ghost_hover(theme))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.submit_dialog(cx);
                                    }))
                                    .child(SharedString::from("Save")),
                            )
                            .child(
                                widgets::ghost_action(theme)
                                    .id("routing-dialog-cancel")
                                    .hover(widgets::ghost_hover(theme))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.dialog = None;
                                        cx.notify();
                                    }))
                                    .child(SharedString::from("Cancel")),
                            ),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(name: &str) -> Route {
        serde_json::from_value(serde_json::json!({
            "name": name,
            "workspace": "w",
            "repo": "/tmp",
            "runtime": "claude-code",
        }))
        .unwrap()
    }

    /// The row's readings of a route (`match_summary`, `cap_summary`) are the
    /// board crate's, tested there — this page renders them rather than
    /// re-deriving them, so the CLI and the settings page cannot disagree about
    /// what a config says.
    #[test]
    fn the_editable_fields_name_their_toml_keys() {
        let r = route("a");
        assert_eq!(RouteField::Account.key(), "account");
        assert_eq!(RouteField::MaxDuration.key(), "max_duration");
        assert_eq!(RouteField::Account.current(&r), None);
        assert_eq!(
            RouteField::Runtime.current(&r).as_deref(),
            Some("claude-code")
        );
    }
}
