//! Settings → Automations (gh#490): the board host's auto-pick rules — what
//! they match, what a dispatch under them runs as, whether they are enabled,
//! paused or unhealthy, and the history of what each rule did and declined to
//! do.
//!
//! Board-hosted like Routing (gh#75), and shaped like it: the rules are
//! `routing.toml`, so every edit here is a `WriteBoardConfig` op through the
//! board crate's validating writer — it has to parse, it has to validate, and
//! the previous contents land in `routing.toml.bak`. What the config cannot
//! carry — live counts, budget meters, health, the run history — arrives by
//! `ReadBoardAutomations` from the same host the config sweep settled on.
//!
//! Two ceremonies are deliberate. **Enabling** a rule opens a confirmation
//! naming exactly what is being authorized — dispatching agents without a
//! keypress, spending the named account — because enabling *is* the human
//! authorization every later dispatch rides on. **Deleting** confirms too, and
//! says what it does not do: work already running is untouched.

use gpui::{
    AnyElement, Context, Entity, SharedString, Subscription, Task, Window, div, prelude::*, px,
};

use comet_board::autopick::{AutomationsView, RuleStatus};
use comet_board::config::Automation;
use comet_board::db::AutomationLogRow;
use comet_board::routes::RoutingView;
use comet_proto::view::board;
use comet_rpc::methods;
use serde::Deserialize;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

/// What the config methods answer with — the slice this page reads. The same
/// wire shape Routing's page deserializes; declared privately here the way
/// `accounts.rs` declares its own, because the reply is a contract and the
/// struct is just this page's reading of it.
#[derive(Debug, Clone, Deserialize)]
struct BoardConfig {
    routing: RoutingView,
    /// The furniture question (gh#434), riding the config reply so the host
    /// sweep can settle on a board somebody has actually released work from.
    #[serde(default)]
    dispatched: bool,
}

/// One rule key this page edits inline. Everything else — a rename, a rule
/// reordering — is a `routing.toml` edit somebody does knowingly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuleField {
    Owner,
    Labels,
    ExcludeLabels,
    Account,
    Runtime,
    Model,
    Route,
    MaxPerEval,
    MaxConcurrent,
    DailyBudget,
    Cooldown,
}

impl RuleField {
    fn key(self) -> &'static str {
        match self {
            RuleField::Owner => "owner",
            RuleField::Labels => "labels",
            RuleField::ExcludeLabels => "exclude_labels",
            RuleField::Account => "account",
            RuleField::Runtime => "runtime",
            RuleField::Model => "model",
            RuleField::Route => "route",
            RuleField::MaxPerEval => "max_per_eval",
            RuleField::MaxConcurrent => "max_concurrent",
            RuleField::DailyBudget => "daily_budget",
            RuleField::Cooldown => "cooldown",
        }
    }

    fn label(self) -> &'static str {
        match self {
            RuleField::Owner => "Owner",
            RuleField::Labels => "Required labels (comma-separated)",
            RuleField::ExcludeLabels => "Excluded labels (comma-separated)",
            RuleField::Account => "Billing account (slot id)",
            RuleField::Runtime => "Runtime",
            RuleField::Model => "Model",
            RuleField::Route => "Route",
            RuleField::MaxPerEval => "Max dispatches per evaluation",
            RuleField::MaxConcurrent => "Max live attempts",
            RuleField::DailyBudget => "Daily dispatch budget",
            RuleField::Cooldown => "Failure cooldown",
        }
    }

    /// What an empty field means, said before somebody clears one.
    fn empty_hint(self) -> &'static str {
        match self {
            RuleField::Owner => "Required to enable — the human this automation answers to.",
            RuleField::Labels => "Required to enable — the label that means \
                                  \"approved for autonomous execution\".",
            RuleField::ExcludeLabels => "Empty: nothing excluded.",
            RuleField::Account => "Required — without it every candidate is refused. \
                                   `comet-board doctor` lists this device's slot ids.",
            RuleField::Runtime => "Empty: the route's runtime.",
            RuleField::Model => "Empty: the harness default.",
            RuleField::Route => "Empty: whatever route each task resolves to.",
            RuleField::MaxPerEval => "Empty: 1 per evaluation.",
            RuleField::MaxConcurrent => "Empty: 1 live attempt.",
            RuleField::DailyBudget => "Empty: no daily cap (concurrency still holds).",
            RuleField::Cooldown => "Empty: 30m. `off` reconsiders on the next cycle.",
        }
    }

    fn current(self, rule: &Automation) -> Option<String> {
        let list = |labels: &[String]| {
            (!labels.is_empty()).then(|| labels.join(", "))
        };
        match self {
            RuleField::Owner => rule.owner.clone(),
            RuleField::Labels => list(&rule.labels),
            RuleField::ExcludeLabels => list(&rule.exclude_labels),
            RuleField::Account => rule.account.clone(),
            RuleField::Runtime => rule.runtime.clone(),
            RuleField::Model => rule.model.clone(),
            RuleField::Route => rule.route.clone(),
            RuleField::MaxPerEval => Some(rule.max_per_eval.to_string()),
            RuleField::MaxConcurrent => Some(rule.max_concurrent.to_string()),
            RuleField::DailyBudget => rule.daily_budget.map(|b| b.to_string()),
            RuleField::Cooldown => Some(rule.cooldown.clone()),
        }
    }
}

struct FieldDialog {
    rule: String,
    field: RuleField,
    input: Entity<ComposerInput>,
    _events: Subscription,
}

/// The two confirmations, one shape: what is about to happen, said in full,
/// with the one verb that does it.
enum Confirm {
    /// Enabling is the authorization (gh#490) — the dialog names the agents,
    /// the account and the owner it authorizes.
    Enable { rule: String, summary: String },
    Delete { rule: String },
}

struct NewRule {
    name: Entity<ComposerInput>,
    _events: Subscription,
}

pub struct AutomationsPage {
    state: Entity<AppState>,
    /// Which device's board — `None` is this one. The same sweep Routing uses.
    host: Option<String>,
    config: Option<BoardConfig>,
    automations: Option<AutomationsView>,
    dialog: Option<FieldDialog>,
    confirm: Option<Confirm>,
    new_rule: Option<NewRule>,
    error: Option<SharedString>,
    busy: bool,
    loaded: bool,
    task: Option<Task<()>>,
    fetch_task: Option<Task<()>>,
    _observe: Subscription,
}

impl AutomationsPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let mut page = Self {
            state,
            host: None,
            config: None,
            automations: None,
            dialog: None,
            confirm: None,
            new_rule: None,
            error: None,
            busy: false,
            loaded: false,
            task: None,
            fetch_task: None,
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

    /// Find the board host and read its config — the same sweep and the same
    /// furniture rule as the Routing page (gh#434) — then fetch the rules'
    /// health and history from whichever host settled.
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
            let mut settled: Option<(Option<String>, BoardConfig)> = None;
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
                            settled = Some((candidate, config));
                            break;
                        }
                        Ok(config) => {
                            if furniture.is_none() {
                                furniture = Some((candidate, config));
                            }
                        }
                        Err(err) => last = Some(format!("Unreadable config: {err}")),
                    },
                    Err(err) => last = Some(err.to_string()),
                }
            }
            this.update(cx, |page, cx| {
                page.loaded = true;
                match settled.or(furniture) {
                    Some((host, config)) => {
                        page.host = host;
                        page.config = Some(config);
                        page.fetch_automations(cx);
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

    /// Read the rules' health and history off the settled host.
    fn fetch_automations(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.host_params(serde_json::json!({}));
        self.fetch_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::READ_BOARD_AUTOMATIONS, params)
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(value) => match serde_json::from_value::<AutomationsView>(value) {
                        Ok(view) => page.automations = Some(view),
                        Err(err) => {
                            page.error = Some(format!("Unreadable automations: {err}").into())
                        }
                    },
                    Err(err) => page.error = Some(format!("{err}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Send one write; the reply replaces the config, and the health/history
    /// view is re-read — a write that flipped `enabled` changes both.
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
                        Ok(config) => {
                            page.config = Some(config);
                            page.fetch_automations(cx);
                        }
                        Err(err) => page.error = Some(format!("Unreadable reply: {err}").into()),
                    },
                    // The refusals are written to be read: "automation `x` is
                    // enabled with no owner; …".
                    Err(err) => page.error = Some(format!("{err}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn open_dialog(&mut self, rule: String, field: RuleField, cx: &mut Context<Self>) {
        let current = self
            .rule_config(&rule)
            .and_then(|r| field.current(r));
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
            rule,
            field,
            input,
            _events: events,
        });
        self.confirm = None;
        cx.notify();
    }

    fn submit_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.dialog.take() else {
            return;
        };
        let text = dialog.input.read(cx).text().trim().to_string();
        // An emptied field is a removal: the key falls back to its default,
        // which is what the dialog's hint said clearing means.
        let value = (!text.is_empty()).then_some(text);
        self.write(
            serde_json::json!({
                "op": "automation",
                "automation": dialog.rule,
                "key": dialog.field.key(),
                "value": value,
            }),
            cx,
        );
    }

    fn rule_config(&self, name: &str) -> Option<&Automation> {
        self.config
            .as_ref()?
            .routing
            .config
            .as_ref()?
            .automations
            .iter()
            .find(|a| a.name == name)
    }

    fn open_new_rule(&mut self, cx: &mut Context<Self>) {
        let name = cx.new(|cx| ComposerInput::new("Rule name", cx));
        let events = cx.subscribe(&name, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_new_rule(cx);
            }
        });
        self.new_rule = Some(NewRule {
            name,
            _events: events,
        });
        cx.notify();
    }

    fn submit_new_rule(&mut self, cx: &mut Context<Self>) {
        let Some(flow) = self.new_rule.take() else {
            return;
        };
        let name = flow.name.read(cx).text().trim().to_string();
        if name.is_empty() {
            self.error = Some("A rule needs a name".into());
            cx.notify();
            return;
        }
        self.write(
            serde_json::json!({ "op": "automationAdd", "name": name }),
            cx,
        );
    }

    /// Enable ceremony: everything the click would authorize, in one sentence,
    /// before the write that authorizes it.
    fn open_enable_confirm(&mut self, status: &RuleStatus, cx: &mut Context<Self>) {
        let rule = &status.rule;
        let mut parts: Vec<String> = Vec::new();
        parts.push(format!(
            "tasks labeled {}",
            if rule.labels.is_empty() {
                "(none set)".to_string()
            } else {
                rule.labels.join(" + ")
            }
        ));
        if let Some(source) = &rule.source {
            parts.push(format!("from {source}"));
        }
        if let Some(runtime) = &rule.runtime {
            parts.push(format!("as {runtime}"));
        }
        if let Some(model) = &rule.model {
            parts.push(format!("on {model}"));
        }
        parts.push(match &rule.account {
            Some(account) => format!("billed to account {account}"),
            None => "with NO billing account (every candidate will be refused)".to_string(),
        });
        parts.push(format!("up to {} live", rule.max_concurrent.max(1)));
        let summary = format!(
            "Enabling “{}” authorizes the board to dispatch agents without a keypress: {}. \
             Owned by {}.",
            rule.name,
            parts.join(", "),
            rule.owner.as_deref().unwrap_or("nobody — set an owner first"),
        );
        self.confirm = Some(Confirm::Enable {
            rule: rule.name.clone(),
            summary,
        });
        self.dialog = None;
        cx.notify();
    }

    fn set_enabled(&mut self, rule: String, enabled: bool, cx: &mut Context<Self>) {
        self.confirm = None;
        self.write(
            serde_json::json!({
                "op": "automation",
                "automation": rule,
                "key": "enabled",
                "value": if enabled { "true" } else { "false" },
            }),
            cx,
        );
    }

    fn duplicate(&mut self, rule: &str, cx: &mut Context<Self>) {
        // `<name> copy`, `<name> copy 2`, … — the writer refuses duplicates,
        // so probe against the config we hold.
        let existing: Vec<String> = self
            .config
            .as_ref()
            .and_then(|c| c.routing.config.as_ref())
            .map(|c| c.automations.iter().map(|a| a.name.clone()).collect())
            .unwrap_or_default();
        let mut candidate = format!("{rule} copy");
        let mut n = 2;
        while existing.iter().any(|e| e.eq_ignore_ascii_case(&candidate)) {
            candidate = format!("{rule} copy {n}");
            n += 1;
        }
        self.write(
            serde_json::json!({ "op": "automationAdd", "name": candidate, "from": rule }),
            cx,
        );
    }

    fn delete(&mut self, rule: String, cx: &mut Context<Self>) {
        self.confirm = None;
        self.write(
            serde_json::json!({ "op": "automationRemove", "name": rule }),
            cx,
        );
    }
}

impl Render for AutomationsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let host = self.host_label(cx);
        let busy = self.busy;
        let view = self.automations.clone();

        let mut column = widgets::page_column()
            .child(widgets::page_header(
                &theme,
                "Automations",
                view.as_ref().map(|v| v.rules.len()),
            ))
            .child(widgets::page_subtitle(
                &theme,
                match &view {
                    Some(view) => {
                        let next = view
                            .next_eval_secs
                            .map(|s| format!(" · next check in {}", board::format_age(s as i64)))
                            .unwrap_or_default();
                        format!(
                            "Auto-pick rules on {host} — an enabled rule dispatches ready, \
                             labeled tasks without a keypress{next}."
                        )
                    }
                    None if self.loaded => "No board found".to_string(),
                    None => "Reading…".to_string(),
                },
            ));

        if let Some(error) = self.error.clone() {
            column = column.child(
                widgets::error_strip(&theme, error)
                    .id("automations-error")
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.error = None;
                        cx.notify();
                    })),
            );
        }

        column = column.child(self.render_new_rule(&theme, busy, cx));

        if let Some(view) = &view {
            let mut card = widgets::section_card(&theme);
            if view.rules.is_empty() {
                card = card.child(
                    widgets::card_row(&theme, true).child(
                        div()
                            .text_size(px(Theme::TEXT_DENSE))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(
                                "No rules yet — the board dispatches nothing on its own.",
                            )),
                    ),
                );
            }
            for (ix, status) in view.rules.iter().enumerate() {
                card = card.child(self.render_rule(&theme, ix, status, busy, cx));
            }
            column = column.child(card);

            if let Some(confirm) = &self.confirm {
                column = column.child(self.render_confirm(&theme, confirm, busy, cx));
            }
            if let Some(dialog) = &self.dialog {
                column = column.child(self.render_dialog(&theme, dialog));
            }

            column = column
                .child(
                    div()
                        .mt(px(28.0))
                        .text_size(px(Theme::TEXT_BODY))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.text)
                        .child(SharedString::from("Recent activity")),
                )
                .child(widgets::page_subtitle(
                    &theme,
                    "Why every considered task was dispatched, skipped, deferred or refused. \
                     Repeats collapse into one line with a count.",
                ))
                .child(render_history(&theme, &view.recent));
        }

        div()
            .id("automations-page")
            .size_full()
            .overflow_y_scroll()
            .child(column)
    }
}

impl AutomationsPage {
    fn render_new_rule(&self, theme: &Theme, busy: bool, cx: &mut Context<Self>) -> AnyElement {
        let Some(flow) = &self.new_rule else {
            return widgets::section_card(theme)
                .child(
                    widgets::card_row(theme, true)
                        .child(widgets::row_tile(theme, crate::icons::MAGIC_STICK))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .child(widgets::row_title(theme, "New rule…"))
                                .child(widgets::meta_line(
                                    theme,
                                    vec![
                                        div()
                                            .min_w_0()
                                            .child(SharedString::from(
                                                "Created paused — fill in its match and \
                                                 dispatch settings, then enable it",
                                            ))
                                            .into_any_element(),
                                    ],
                                )),
                        )
                        .child(
                            widgets::ghost_action(theme)
                                .id("automation-new-open")
                                .hover(widgets::ghost_hover(theme))
                                .when(busy, |el| el.opacity(0.4))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if this.busy {
                                        return;
                                    }
                                    this.open_new_rule(cx);
                                }))
                                .child(SharedString::from("New rule")),
                        ),
                )
                .into_any_element();
        };

        widgets::section_card(theme)
            .child(
                widgets::card_row(theme, true)
                    .flex_col()
                    .items_start()
                    .gap(px(8.0))
                    .child(widgets::row_title(theme, "New rule"))
                    .child(div().w_full().child(flow.name.clone()))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.0))
                            .child(
                                widgets::ghost_action(theme)
                                    .id("automation-new-create")
                                    .hover(widgets::ghost_hover(theme))
                                    .when(busy, |el| el.opacity(0.4))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if this.busy {
                                            return;
                                        }
                                        this.submit_new_rule(cx);
                                    }))
                                    .child(SharedString::from("Create")),
                            )
                            .child(
                                widgets::ghost_action(theme)
                                    .id("automation-new-cancel")
                                    .hover(widgets::ghost_hover(theme))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.new_rule = None;
                                        cx.notify();
                                    }))
                                    .child(SharedString::from("Cancel")),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_rule(
        &self,
        theme: &Theme,
        ix: usize,
        status: &RuleStatus,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rule = &status.rule;
        let name = rule.name.clone();

        let state_badge = match status.state.as_str() {
            "enabled" => widgets::badge_active(theme, "enabled").into_any_element(),
            "unhealthy" => div()
                .flex_none()
                .px(px(8.0))
                .py(px(1.0))
                .rounded(px(Theme::RADIUS_CHIP))
                .bg(theme.warning.opacity(0.14))
                .text_size(px(Theme::TEXT_CAPTION))
                .text_color(theme.warning_text())
                .child(SharedString::from("unhealthy"))
                .into_any_element(),
            _ => widgets::badge(theme, "paused").into_any_element(),
        };

        let mut meta: Vec<AnyElement> = Vec::new();
        meta.push(
            div()
                .child(SharedString::from(format!(
                    "owned by {}",
                    rule.owner.as_deref().unwrap_or("nobody")
                )))
                .into_any_element(),
        );
        meta.push(
            div()
                .min_w_0()
                .truncate()
                .child(SharedString::from(if rule.labels.is_empty() {
                    "no labels".to_string()
                } else {
                    format!("labels: {}", rule.labels.join(", "))
                }))
                .into_any_element(),
        );
        meta.push(
            div()
                .child(SharedString::from(format!(
                    "{} live · {} today",
                    status.live, status.dispatched_last_day
                )))
                .into_any_element(),
        );
        if let Some(event) = &status.last_event {
            meta.push(
                div()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(format!(
                        "last: {} {}",
                        event.identifier.as_deref().unwrap_or(""),
                        event.decision
                    )))
                    .into_any_element(),
            );
        }

        let field_chip = |field: RuleField, label: SharedString| {
            let name = name.clone();
            widgets::ghost_action(theme)
                .id(("automation-field", ix * 16 + field.key().len()))
                .hover(widgets::ghost_hover(theme))
                .when(busy, |el| el.opacity(0.4))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.busy {
                        return;
                    }
                    this.open_dialog(name.clone(), field, cx);
                }))
                .child(label)
        };
        let short = |value: &Option<String>, fallback: &str| -> String {
            value.clone().unwrap_or_else(|| fallback.to_string())
        };

        let toggle: AnyElement = if rule.enabled {
            let name = name.clone();
            widgets::ghost_action(theme)
                .id(("automation-pause", ix))
                .hover(widgets::ghost_hover(theme))
                .when(busy, |el| el.opacity(0.4))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.busy {
                        return;
                    }
                    // Pausing needs no ceremony: it only stops future
                    // dispatches, and stopping is never the risky direction.
                    this.set_enabled(name.clone(), false, cx);
                }))
                .child(SharedString::from("Pause"))
                .into_any_element()
        } else {
            let status = status.clone();
            widgets::ghost_action(theme)
                .id(("automation-enable", ix))
                .hover(widgets::ghost_hover(theme))
                .when(busy, |el| el.opacity(0.4))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.busy {
                        return;
                    }
                    this.open_enable_confirm(&status, cx);
                }))
                .child(SharedString::from("Enable…"))
                .into_any_element()
        };

        let mut problems = div().flex().flex_col().gap(px(4.0)).w_full();
        for (pix, problem) in status.problems.iter().enumerate() {
            problems = problems.child(
                widgets::warning_strip(theme, problem.clone())
                    .mt(px(6.0))
                    .id(("automation-problem", ix * 8 + pix)),
            );
        }

        let dup_name = name.clone();
        let del_name = name.clone();
        widgets::card_row(theme, ix == 0)
            .id(("automation", ix))
            .items_start()
            .child(widgets::row_tile(theme, crate::icons::MAGIC_STICK))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .child(widgets::row_title(theme, rule.name.clone()))
                            .child(state_badge),
                    )
                    .child(widgets::meta_line(theme, meta))
                    .child(problems)
                    .child(
                        div()
                            .mt(px(6.0))
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap(px(4.0))
                            .child(field_chip(
                                RuleField::Labels,
                                format!(
                                    "labels: {}",
                                    if rule.labels.is_empty() {
                                        "—".to_string()
                                    } else {
                                        rule.labels.join(", ")
                                    }
                                )
                                .into(),
                            ))
                            .child(field_chip(
                                RuleField::ExcludeLabels,
                                format!(
                                    "exclude: {}",
                                    if rule.exclude_labels.is_empty() {
                                        "—".to_string()
                                    } else {
                                        rule.exclude_labels.join(", ")
                                    }
                                )
                                .into(),
                            ))
                            .child(field_chip(
                                RuleField::Owner,
                                format!("owner: {}", short(&rule.owner, "—")).into(),
                            ))
                            .child(field_chip(
                                RuleField::Account,
                                format!("account: {}", short(&rule.account, "—")).into(),
                            ))
                            .child(field_chip(
                                RuleField::Runtime,
                                format!("runtime: {}", short(&rule.runtime, "route's")).into(),
                            ))
                            .child(field_chip(
                                RuleField::Model,
                                format!("model: {}", short(&rule.model, "default")).into(),
                            ))
                            .child(field_chip(
                                RuleField::Route,
                                format!("route: {}", short(&rule.route, "any")).into(),
                            ))
                            .child(field_chip(
                                RuleField::MaxPerEval,
                                format!("per eval: {}", rule.max_per_eval.max(1)).into(),
                            ))
                            .child(field_chip(
                                RuleField::MaxConcurrent,
                                format!("concurrent: {}", rule.max_concurrent.max(1)).into(),
                            ))
                            .child(field_chip(
                                RuleField::DailyBudget,
                                format!(
                                    "budget: {}",
                                    rule.daily_budget
                                        .map(|b| format!("{b}/day"))
                                        .unwrap_or_else(|| "—".into())
                                )
                                .into(),
                            ))
                            .child(field_chip(
                                RuleField::Cooldown,
                                format!("cooldown: {}", rule.cooldown).into(),
                            )),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap(px(4.0))
                    .child(toggle)
                    .child(
                        widgets::ghost_action(theme)
                            .id(("automation-duplicate", ix))
                            .hover(widgets::ghost_hover(theme))
                            .when(busy, |el| el.opacity(0.4))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.busy {
                                    return;
                                }
                                this.duplicate(&dup_name.clone(), cx);
                            }))
                            .child(SharedString::from("Duplicate")),
                    )
                    .child(
                        widgets::ghost_action(theme)
                            .id(("automation-delete", ix))
                            .hover(widgets::ghost_hover(theme))
                            .when(busy, |el| el.opacity(0.4))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.busy {
                                    return;
                                }
                                this.confirm = Some(Confirm::Delete {
                                    rule: del_name.clone(),
                                });
                                this.dialog = None;
                                cx.notify();
                            }))
                            .child(SharedString::from("Delete…")),
                    ),
            )
            .into_any_element()
    }

    fn render_confirm(
        &self,
        theme: &Theme,
        confirm: &Confirm,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (copy, verb, action): (String, &str, Box<dyn Fn(&mut Self, &mut Context<Self>)>) =
            match confirm {
                Confirm::Enable { rule, summary } => {
                    let rule = rule.clone();
                    (
                        summary.clone(),
                        "Enable",
                        Box::new(move |this, cx| this.set_enabled(rule.clone(), true, cx)),
                    )
                }
                Confirm::Delete { rule } => {
                    let name = rule.clone();
                    (
                        format!(
                            "Delete “{name}”? Future dispatches stop; work already running \
                             is not cancelled, and its history stays on this page until it \
                             ages out."
                        ),
                        "Delete",
                        Box::new(move |this, cx| this.delete(name.clone(), cx)),
                    )
                }
            };
        widgets::section_card(theme)
            .child(
                widgets::card_row(theme, true)
                    .flex_col()
                    .items_start()
                    .gap(px(8.0))
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_DENSE))
                            .line_height(px(18.0))
                            .text_color(theme.text)
                            .child(SharedString::from(copy)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(8.0))
                            .child(
                                widgets::ghost_action(theme)
                                    .id("automation-confirm")
                                    .hover(widgets::ghost_hover(theme))
                                    .when(busy, |el| el.opacity(0.4))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if this.busy {
                                            return;
                                        }
                                        action(this, cx);
                                    }))
                                    .child(SharedString::from(verb.to_string())),
                            )
                            .child(
                                widgets::ghost_action(theme)
                                    .id("automation-confirm-cancel")
                                    .hover(widgets::ghost_hover(theme))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.confirm = None;
                                        cx.notify();
                                    }))
                                    .child(SharedString::from("Cancel")),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_dialog(&self, theme: &Theme, dialog: &FieldDialog) -> AnyElement {
        widgets::section_card(theme)
            .child(
                widgets::card_row(theme, true)
                    .flex_col()
                    .items_start()
                    .gap(px(8.0))
                    .child(widgets::row_title(
                        theme,
                        format!("{} — {}", dialog.rule, dialog.field.label()),
                    ))
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(theme.text_subtle)
                            .child(SharedString::from(dialog.field.empty_hint())),
                    )
                    .child(div().w_full().child(dialog.input.clone())),
            )
            .into_any_element()
    }
}

/// The history card: one line per streak, newest first.
fn render_history(theme: &Theme, rows: &[AutomationLogRow]) -> AnyElement {
    let mut card = widgets::section_card(theme);
    if rows.is_empty() {
        card = card.child(
            widgets::card_row(theme, true).child(
                div()
                    .text_size(px(Theme::TEXT_DENSE))
                    .text_color(theme.text_muted)
                    .child(SharedString::from("Nothing yet.")),
            ),
        );
    }
    for (ix, row) in rows.iter().enumerate() {
        let age = chrono::DateTime::parse_from_rfc3339(&row.last_at)
            .map(|at| {
                let secs = (chrono::Utc::now() - at.with_timezone(&chrono::Utc)).num_seconds();
                format!("{} ago", board::format_age(secs))
            })
            .unwrap_or_else(|_| row.last_at.clone());
        let subject = row.identifier.clone().or(row.task_id.clone());
        let count = (row.count > 1)
            .then(|| format!(" ×{}", row.count))
            .unwrap_or_default();
        let line = format!(
            "{}{} — {}{}: {}",
            row.rule,
            subject.map(|s| format!(" · {s}")).unwrap_or_default(),
            row.decision,
            count,
            row.reason
        );
        card = card.child(
            widgets::card_row(theme, ix == 0)
                .items_start()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(Theme::TEXT_DENSE))
                        .line_height(px(18.0))
                        .text_color(theme.text_subtle)
                        .child(SharedString::from(line)),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(theme.text_faint)
                        .child(SharedString::from(age)),
                ),
        );
    }
    card.into_any_element()
}
