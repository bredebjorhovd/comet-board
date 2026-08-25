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
//! [`comet_proto::view::board::host_candidates`], which needs no configuration
//! and no coupling to whether the board panel has ever been opened — and it
//! settles the way the panel does (gh#125, taught to this page by gh#434): a
//! config from a board nobody has ever released work from is *furniture*, held
//! as a fallback while the sweep keeps asking, and shown only when no board
//! with dispatch evidence answers. Local answers first because asking locally
//! is free; without the rule, a leftover local `board.db` would be the config
//! this page reads — and writes routes into.

use gpui::{
    AnyElement, Context, Entity, SharedString, Subscription, Task, Window, div, prelude::*, px,
};

use comet_board::adopt::Unadopted;
use comet_board::config::Route;
use comet_board::dispatch::space_matches;
use comet_board::onboard::{Candidate, Onboarded};
use comet_board::routes::{RoutingView, cap_summary, match_summary};
use comet_proto::McpServer;
use comet_proto::view::board;
use comet_rpc::methods;
use serde::Deserialize;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::mcp::{
    BUILTIN_ARGS, BUILTIN_COMMAND, BUILTIN_NAME, CommandLocations, McpRowInputs, collect_servers,
    route_mcp_summary,
};
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

/// What both config methods answer with.
#[derive(Debug, Clone, Deserialize)]
struct BoardConfig {
    routing: RoutingView,
    #[serde(default)]
    unadopted: Vec<Unadopted>,
    /// Whether anybody has ever released work from this board (gh#434) — the
    /// board pane's furniture question, riding the config reply so the host
    /// sweep can ask it without a second call. Defaulted on the wire: an
    /// older board answers without it and reads as furniture, which costs it
    /// only the tie against a board that says otherwise.
    #[serde(default)]
    dispatched: bool,
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

/// The per-route MCP servers editor (gh#606): rows of (name · command · args)
/// written through the board's validating writer as one structured op — never
/// raw TOML.
///
/// The rows are seeded with the list the route *resolves to* (its own, else
/// `[defaults]`), so "customize what you have" costs nothing; saving always
/// writes an explicit list for the route, and the reset action removes the
/// key, falling back to `[defaults]` — the same two states
/// [`comet_board::config::Route::mcp_servers`] has on the wire.
struct McpEditor {
    route: usize,
    route_name: String,
    workspace: String,
    /// What the route itself declares. `None` is inheriting: the caption says
    /// so and says what it is inheriting, because a list on screen that comes
    /// from somewhere else must not read as this route's own.
    declared: Option<Vec<McpServer>>,
    /// The defaults' resolved list, shown beside the inherit note.
    inherited: Vec<McpServer>,
    rows: Vec<McpRowInputs>,
    /// Command resolution on the route's workspace device: trimmed command →
    /// where it was found, or `None` when nowhere. Commands never checked yet
    /// are simply absent, so a stale answer can't dress up as fresh.
    located: std::collections::HashMap<String, Option<String>>,
    /// The device the answers above describe — named in the warnings, since
    /// "not found" without "where looked" is half a diagnosis.
    device_label: Option<String>,
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
    /// The open MCP editor, if any (one at a time; it is a card on the page).
    mcp_editor: Option<McpEditor>,
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
    /// Command-existence checks (gh#606) run on their own slot too — a keystroke
    /// in a command field replaces the check in flight rather than queueing
    /// one per character.
    locate_task: Option<Task<()>>,
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
            mcp_editor: None,
            onboard: None,
            onboarded: None,
            error: None,
            busy: false,
            busy_note: None,
            loaded: false,
            task: None,
            onboard_task: None,
            locate_task: None,
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
    ///
    /// A candidate that answers **without dispatch evidence** is furniture
    /// (gh#434): its config is held as a fallback while the sweep keeps
    /// asking, and settles only when no board somebody has released work from
    /// answers. The sweep used to stop on the first answer, and local is
    /// asked first — so a stale local board's `routing.toml` was what a Mac
    /// read, and what its edits were written into, whenever a local board
    /// existed at all.
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
            let mut furniture: Option<(Option<String>, BoardConfig)> = None;
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
                    Ok(value) => match serde_json::from_value::<BoardConfig>(value) {
                        Ok(config) if config.dispatched => {
                            let _ = this.update(cx, |page, cx| {
                                page.loaded = true;
                                page.host = candidate;
                                page.config = Some(config);
                                cx.notify();
                            });
                            return;
                        }
                        // First held answer wins the fallback slot — it is
                        // the earliest in sweep order, which is the old
                        // tie-break.
                        Ok(config) => {
                            if furniture.is_none() {
                                furniture = Some((candidate, config));
                            }
                        }
                        // A board that answered unreadably has still
                        // answered, but there is nothing to draw from it —
                        // carry it as the error the page falls back to.
                        Err(err) => last = Some(format!("Unreadable config: {err}")),
                    },
                    Err(err) => last = Some(err.to_string()),
                }
            }
            this.update(cx, |page, cx| {
                page.loaded = true;
                // Everyone has been asked. A config held for want of dispatch
                // evidence is the best answer there is — settle on it.
                match furniture {
                    Some((host, config)) => {
                        page.host = host;
                        page.config = Some(config);
                    }
                    None => {
                        page.error = Some(
                            match last {
                                Some(err) => format!("No device here hosts a board ({err})"),
                                None => "No device here hosts a board".to_string(),
                            }
                            .into(),
                        );
                    }
                }
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

    // ---- the per-route MCP servers editor (gh#606) ----

    /// Open the editor on `route`, seeded with what that route resolves to —
    /// its own list when it has one, `[defaults]` when it inherits.
    fn open_mcp_editor(&mut self, route_ix: usize, cx: &mut Context<Self>) {
        let Some(config) = self.config.as_ref() else {
            return;
        };
        let Some(parsed) = config.routing.config.as_ref() else {
            return;
        };
        let Some(route) = parsed.routes.get(route_ix) else {
            return;
        };
        let declared = route.mcp_servers.clone();
        let inherited = parsed.defaults.mcp_servers.clone();
        let seed = declared.as_deref().unwrap_or(&inherited);
        let mut rows: Vec<McpRowInputs> = seed
            .iter()
            .map(|server| {
                McpRowInputs::new(&server.name, &server.command, &server.args.join(" "), cx)
            })
            .collect();
        for row in &mut rows {
            Self::subscribe_row(row, cx);
        }
        let editor = McpEditor {
            route: route_ix,
            route_name: route.display_name().to_string(),
            workspace: route.workspace.clone(),
            declared,
            inherited,
            rows,
            located: std::collections::HashMap::new(),
            device_label: None,
        };
        self.mcp_editor = Some(editor);
        self.error = None;
        // The first sanity check is free to run now: a typo'd command should
        // be news before anybody presses Save.
        self.check_mcp_commands(cx);
        cx.notify();
    }

    fn subscribe_row(row: &mut McpRowInputs, cx: &mut Context<Self>) {
        for input in [&row.name, &row.command, &row.args] {
            let event = cx.subscribe(input, |this: &mut Self, _, event, cx| {
                if matches!(event, ComposerInputEvent::Edited(_)) {
                    this.check_mcp_commands(cx);
                }
            });
            row.events.push(event);
        }
    }

    fn close_mcp_editor(&mut self, cx: &mut Context<Self>) {
        self.mcp_editor = None;
        cx.notify();
    }

    /// The servers the rows currently spell, trimmed and split exactly as the
    /// validating writer would parse them back.
    fn mcp_servers_from_rows(&self, cx: &gpui::App) -> Vec<McpServer> {
        let Some(editor) = self.mcp_editor.as_ref() else {
            return Vec::new();
        };
        let tuples: Vec<(String, String, String)> =
            editor.rows.iter().map(|row| row.values(cx)).collect();
        collect_servers(
            &tuples
                .iter()
                .map(|(n, c, a)| (n.as_str(), c.as_str(), a.as_str()))
                .collect::<Vec<_>>(),
        )
    }

    /// What the board's own validator says about the rows — the very function
    /// the write seam refuses on, so a warning here is a refusal there.
    fn mcp_row_problems(&self, cx: &gpui::App) -> Vec<String> {
        let Some(editor) = self.mcp_editor.as_ref() else {
            return Vec::new();
        };
        comet_board::config::mcp_server_problems(
            &format!("route {}", editor.route + 1),
            &self.mcp_servers_from_rows(cx),
        )
    }

    fn mcp_add_row(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.mcp_editor.as_mut() else {
            return;
        };
        let mut row = McpRowInputs::new("", "", "", cx);
        Self::subscribe_row(&mut row, cx);
        editor.rows.push(row);
        cx.notify();
    }

    /// One click for the server every route usually wants: the board's own
    /// dispatch seam, which needs no arguments.
    fn mcp_add_builtin(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.mcp_editor.as_mut() else {
            return;
        };
        if editor
            .rows
            .iter()
            .any(|row| row.values(cx).0.trim().eq_ignore_ascii_case(BUILTIN_NAME))
        {
            return;
        }
        let mut row = McpRowInputs::new(BUILTIN_NAME, BUILTIN_COMMAND, &BUILTIN_ARGS.join(" "), cx);
        Self::subscribe_row(&mut row, cx);
        editor.rows.push(row);
        cx.notify();
    }

    fn mcp_remove_row(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(editor) = self.mcp_editor.as_mut() else {
            return;
        };
        editor.rows.remove(ix);
        self.check_mcp_commands(cx);
        cx.notify();
    }

    fn mcp_move_row(&mut self, ix: usize, delta: isize, cx: &mut Context<Self>) {
        let Some(editor) = self.mcp_editor.as_mut() else {
            return;
        };
        let to = ix as isize + delta;
        if to < 0 || to as usize >= editor.rows.len() {
            return;
        }
        let to = to as usize;
        editor.rows.swap(ix, to);
        cx.notify();
    }

    /// Write the rows as the route's explicit list. The writer re-validates;
    /// the page's own strips already showed the same problems, so Save sits
    /// quiet while any stand.
    fn save_mcp_editor(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.mcp_editor.take() else {
            return;
        };
        let servers = self.mcp_servers_from_rows(cx);
        if !comet_board::config::mcp_server_problems("route", &servers).is_empty() {
            self.mcp_editor = Some(editor);
            return;
        }
        self.write(
            serde_json::json!({
                "op": "routeMcp",
                "route": editor.route,
                "servers": servers,
            }),
            cx,
        );
    }

    /// Remove the route's explicit list: back to inheriting `[defaults]`.
    fn reset_mcp_to_default(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.mcp_editor.take() else {
            return;
        };
        self.write(
            serde_json::json!({
                "op": "routeMcp",
                "route": editor.route,
                "servers": serde_json::Value::Null,
            }),
            cx,
        );
    }

    /// Ask the route's workspace device which of the typed commands actually
    /// resolve there. Commands run where the harness child spawns — not where
    /// this window is open, and not necessarily where the board's file lives.
    ///
    /// Fired on open and on every command edit; the single task slot means a
    /// burst of keystrokes leaves one check in flight, and dropping the old
    /// `Task` cancels it.
    fn check_mcp_commands(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(editor) = self.mcp_editor.as_ref() else {
            return;
        };
        let mut commands: Vec<String> = Vec::new();
        let values: Vec<(String, String, String)> =
            editor.rows.iter().map(|row| row.values(cx)).collect();
        for (_, command, _) in &values {
            let command = command.trim().to_string();
            if !command.is_empty() && !commands.contains(&command) {
                commands.push(command);
            }
        }
        if commands.is_empty() {
            return;
        }
        let workspace = editor.workspace.clone();
        let (target, device_label) = {
            let state = self.state.read(cx);
            let space = state
                .spaces
                .iter()
                .find(|s| space_matches(s.name.as_deref(), &s.path, &workspace))
                .map(|s| s.device_id.clone());
            let label = space.as_deref().and_then(|id| {
                state
                    .devices
                    .iter()
                    .find(|d| d.id == id)
                    .map(|d| d.name.clone())
            });
            // This device's engine answers without a relay hop; anything else
            // is addressed by id like every forwardable call.
            let target = match (&space, state.local_device_id.as_deref()) {
                (Some(id), Some(local)) if id == local => None,
                (Some(id), _) => Some(id.clone()),
                (None, _) => None,
            };
            (target, label)
        };
        if let Some(editor) = self.mcp_editor.as_mut() {
            editor.device_label = device_label;
        }
        let mut params = serde_json::json!({ "commands": commands });
        if let (Some(target), Some(object)) = (target, params.as_object_mut()) {
            object.insert("targetDeviceId".into(), serde_json::json!(target));
        }
        self.locate_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::LOCATE_COMMANDS, params).await;
            this.update(cx, |page, cx| {
                let Some(editor) = page.mcp_editor.as_mut() else {
                    return;
                };
                match result {
                    Ok(value) => {
                        if let Some(locations) = CommandLocations::from_reply(&value) {
                            editor.located = locations.found;
                        }
                    }
                    // A failed check stays silent on the page: no answer is
                    // not a "not found", and inventing one would warn about
                    // the network instead of the command.
                    Err(err) => tracing::warn!(error = %err, "LocateCommands failed"),
                }
                cx.notify();
            })
            .ok();
        }));
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
                widgets::error_strip(&theme, error)
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
                column = column.child(
                    widgets::warning_strip(&theme, problem.clone())
                        // The strip carries no margin of its own (gh#277): a
                        // section stacks it on the section's own gap, and a
                        // page that appends one to a gapless column says so.
                        .mt(px(widgets::HEADER_GAP))
                        .id(("routing-problem", ix)),
                );
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

        // The MCP editor renders like the field dialogs do: a card at the
        // bottom of the page, close to where the write's refusal strip lands.
        if let Some(editor) = self.mcp_editor.as_ref() {
            column = column.child(self.render_mcp_editor(&theme, editor, cx));
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
        let mcp: SharedString = route_mcp_summary(route.mcp_servers.as_deref()).into();

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
                            ))
                            .child(
                                widgets::ghost_action(theme)
                                    .id(("route-mcp", ix))
                                    .hover(widgets::ghost_hover(theme))
                                    .when(busy, |el| el.opacity(0.4))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if this.busy {
                                            return;
                                        }
                                        this.open_mcp_editor(ix, cx);
                                    }))
                                    .child(format!("mcp: {mcp}")),
                            ),
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

    /// The MCP editor card (gh#606): rows of (name · command · args) with
    /// add / remove / reorder, the built-in server one click away, the same
    /// warnings the writer would raise, a per-harness injection preview, and
    /// the command-not-found check against the route's workspace device.
    fn render_mcp_editor(
        &self,
        theme: &Theme,
        editor: &McpEditor,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let problems = self.mcp_row_problems(cx);
        let servers = self.mcp_servers_from_rows(cx);
        let busy = self.busy;

        let mut card = widgets::section_card(theme).child(
            widgets::card_row(theme, true)
                .flex_col()
                .items_start()
                .gap(px(8.0))
                .child(widgets::row_title(
                    theme,
                    format!("MCP servers — {}", editor.route_name),
                ))
                .child(
                    div()
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(theme.text_subtle)
                        .child(SharedString::from(match editor.declared.as_deref() {
                            // The rows show the defaults' list; saying where
                            // they came from is the difference between
                            // "editing my route" and "about to make this
                            // explicit".
                            None => format!(
                                "Inheriting [defaults] ({} below). Saving writes \
                                 this list as the route's own.",
                                if editor.inherited.is_empty() {
                                    "no servers".to_string()
                                } else {
                                    format!(
                                        "{} server{})",
                                        editor.inherited.len(),
                                        if editor.inherited.len() == 1 { "" } else { "s" }
                                    )
                                }
                            ),
                            Some([]) => "This route opts out — no MCP servers are \
                                 injected on its dispatches."
                                .to_string(),
                            Some(list) => format!(
                                "Custom to this route ({} server{}). Reset removes \
                                 it and falls back to [defaults].",
                                list.len(),
                                if list.len() == 1 { "" } else { "s" }
                            ),
                        })),
                ),
        );

        // The rows.
        for (ix, row) in editor.rows.iter().enumerate() {
            let (_, command, _) = row.values(cx);
            let command_missing = !command.trim().is_empty()
                && editor
                    .located
                    .get(command.trim())
                    .is_some_and(|hit| hit.is_none());
            let mut line = widgets::card_row(theme, false)
                .flex_col()
                .items_start()
                .gap(px(4.0))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_row()
                        .gap(px(4.0))
                        .child(div().w(px(110.0)).child(row.name.clone()))
                        .child(div().flex_1().min_w_0().child(row.command.clone()))
                        .child(div().flex_1().min_w_0().child(row.args.clone())),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(4.0))
                        .when(ix > 0, |el| {
                            el.child(
                                widgets::ghost_action(theme)
                                    .id(("mcp-up", ix))
                                    .hover(widgets::ghost_hover(theme))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.mcp_move_row(ix, -1, cx);
                                    }))
                                    .child(
                                        crate::icons::icon(crate::icons::ARROW_UP).size(px(12.0)),
                                    ),
                            )
                        })
                        .when(ix + 1 < editor.rows.len(), |el| {
                            el.child(
                                widgets::ghost_action(theme)
                                    .id(("mcp-down", ix))
                                    .hover(widgets::ghost_hover(theme))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.mcp_move_row(ix, 1, cx);
                                    }))
                                    .child(
                                        crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                                            .size(px(12.0)),
                                    ),
                            )
                        })
                        .child(
                            widgets::ghost_action(theme)
                                .id(("mcp-remove", ix))
                                .hover(widgets::ghost_hover(theme))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.mcp_remove_row(ix, cx);
                                }))
                                .child(
                                    crate::icons::icon(crate::icons::TRASH_BIN_MINIMALISTIC)
                                        .size(px(12.0)),
                                ),
                        ),
                );
            // The warning rides the row it is about: three servers, one bad
            // command, and a single strip at the bottom would leave somebody
            // counting rows.
            if command_missing {
                let label = editor
                    .device_label
                    .clone()
                    .unwrap_or_else(|| format!("workspace “{}”", editor.workspace));
                line = line.child(
                    div()
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(theme.warning_text())
                        .child(SharedString::from(format!(
                            "`{command}` was not found on {label} — its dispatch would \
                             fail to start this server."
                        ))),
                );
            }
            card = card.child(line);
        }

        // Validation, before the buttons: what Save would be refused for,
        // named while it is still being typed.
        for problem in &problems {
            card = card.child(widgets::warning_strip(theme, problem.clone()));
        }

        // Footer: add affordances left, decisions right.
        card = card.child(
            widgets::card_row(theme, false)
                .flex_col()
                .items_start()
                .gap(px(6.0))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(6.0))
                        .child(
                            widgets::ghost_action(theme)
                                .id("mcp-add-row")
                                .hover(widgets::ghost_hover(theme))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.mcp_add_row(cx);
                                }))
                                .child(crate::icons::icon(crate::icons::PLUS).size(px(12.0)))
                                .child(SharedString::from("Add server")),
                        )
                        .child(
                            widgets::ghost_action(theme)
                                .id("mcp-add-builtin")
                                .hover(widgets::ghost_hover(theme))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.mcp_add_builtin(cx);
                                }))
                                .child(SharedString::from("Add comet-board")),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(6.0))
                        .when(editor.declared.is_some(), |el| {
                            el.child(
                                widgets::ghost_action(theme)
                                    .id("mcp-reset")
                                    .hover(widgets::ghost_hover(theme))
                                    .when(busy, |el| el.opacity(0.4))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if this.busy {
                                            return;
                                        }
                                        this.reset_mcp_to_default(cx);
                                    }))
                                    .child(SharedString::from("Use [defaults]")),
                            )
                        })
                        .child(
                            widgets::ghost_action(theme)
                                .id("mcp-close")
                                .hover(widgets::ghost_hover(theme))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.close_mcp_editor(cx);
                                }))
                                .child(SharedString::from("Cancel")),
                        )
                        .child(
                            widgets::ghost_action(theme)
                                .id("mcp-save")
                                .hover(widgets::ghost_hover(theme))
                                // Quiet rather than hidden while invalid: the
                                // strips above say why, an absent button says
                                // nothing.
                                .when(busy || !problems.is_empty(), |el| el.opacity(0.4))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if this.busy || !this.mcp_row_problems(cx).is_empty() {
                                        return;
                                    }
                                    this.save_mcp_editor(cx);
                                }))
                                .child(SharedString::from("Save")),
                        ),
                ),
        );

        // What each harness would actually receive, rendered by the adapters'
        // own functions so it cannot drift from what spawns. Empty rows mean
        // nothing is injected anywhere — worth saying, not showing.
        card = card.child(self.render_mcp_preview(theme, &servers));
        card.into_any_element()
    }

    /// Per-harness injection previews for `servers`, as caption blocks.
    fn render_mcp_preview(&self, theme: &Theme, servers: &[McpServer]) -> AnyElement {
        let harnesses: [(&str, comet_proto::HarnessId); 3] = [
            ("claude-code", comet_proto::HarnessId::ClaudeCode),
            ("codex", comet_proto::HarnessId::Codex),
            ("opencode", comet_proto::HarnessId::Opencode),
        ];
        let mut block = div().mt(px(4.0)).flex().flex_col().gap(px(4.0)).child(
            div()
                .text_size(px(Theme::TEXT_CAPTION))
                .text_color(theme.text_subtle)
                .child(SharedString::from(
                    "What a run receives from these servers:",
                )),
        );
        for (label, harness) in harnesses {
            match comet_harness::mcp_injection_preview(harness, servers) {
                Some(preview) => {
                    block = block.child(
                        div()
                            .flex()
                            .flex_col()
                            .px(px(8.0))
                            .py(px(6.0))
                            .rounded(px(Theme::RADIUS_ROW))
                            .bg(theme.white_alpha(0.04))
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_CAPTION))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(label)),
                            )
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_CAPTION))
                                    .text_color(theme.text_subtle)
                                    .child(SharedString::from(preview)),
                            ),
                    );
                }
                None => {
                    block = block.child(
                        div()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(theme.text_subtle)
                            .child(SharedString::from(format!("{label}: nothing injected"))),
                    );
                }
            }
        }
        block.into_any_element()
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

    /// An older board answers without the evidence bit and must still parse —
    /// as furniture, which only ever costs it the tie against a board that
    /// says otherwise (gh#434).
    #[test]
    fn a_config_reply_without_the_evidence_bit_reads_as_furniture() {
        let config: BoardConfig = serde_json::from_value(serde_json::json!({
            "routing": {
                "path": "/data/routing.toml",
                "exists": false,
                "text": "",
                "problems": [],
                "backup": false,
            }
        }))
        .unwrap();
        assert!(!config.dispatched);
        assert!(config.unadopted.is_empty());
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
