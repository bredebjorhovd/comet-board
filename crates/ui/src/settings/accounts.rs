//! Settings → Agents / accounts (feature-inventory §1.9): provider cards
//! (Claude Code, Codex) with account rows — email, plan badge, Active, usage
//! meters (neutral → amber ≥80% → red ≥95%, reset time), Switch / Forget —
//! plus the add-account dialogs (paste-code and browser-poll flows) and
//! account-shaped loading skeletons. Comet retargets devices from the settings
//! sidebar (`targetDeviceId` passthrough kept plumbed, unused single-device).
//!
//! The accounts RPC surface is being implemented engine-side in parallel —
//! every call here surfaces failures as inline UI states rather than assuming
//! the methods exist.
//!
//! # What each subscription costs (gh#178)
//!
//! This page also holds the *rates* — the plan cost under each login, which
//! Board stats reads against the list price of the work those logins ran
//! (gh#179, priced by gh#182). It lives here rather than on the stats page for
//! one reason: it is not a fact about the board, it is a fact about a
//! subscription, and the subscription is on the row. **Comet never sees
//! anybody's bill** — this is the one number in the whole config that no
//! amount of probing could produce, so the page asks, says why it is asking,
//! and records an unentered plan as unknown rather than as free.
//!
//! The number lands in the *board's* `routing.toml` (`[account."…"]`), through
//! the same validating writer the Routing page uses, which is why the field
//! only appears when a board answers: the logins are on this device, the
//! ledger is on the box.

use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, Context, Entity, Hsla, SharedString, Subscription, Task, Window, div, prelude::*,
    px,
};
use std::collections::BTreeMap;
use std::time::Duration;

use comet_board::config::{AccountConfig, RoutingConfig};
use comet_proto::view::board;
use comet_proto::view::rates::human_usd;
use comet_proto::{
    AgentAccount, AgentAccountsSnapshot, AgentLoginMode, AgentLoginPoll, AgentLoginStart,
    AgentLoginStatus, HarnessId,
};
use comet_rpc::methods;
use serde::Deserialize;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover::{self, Loadable};
use crate::state::AppState;
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// Pure: usage meters + labels
// ---------------------------------------------------------------------------

pub const USAGE_WARN_FRACTION: f32 = 0.80;
pub const USAGE_CRITICAL_FRACTION: f32 = 0.95;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageLevel {
    /// < 80% — no hue at all. Normal is not a state (see [`usage_color`]).
    Normal,
    /// ≥ 80% — the working hue.
    Warn,
    /// ≥ 95% — the blocked hue.
    Critical,
}

/// Threshold classification of a usage fraction. Pure.
pub fn usage_level(fraction: f32) -> UsageLevel {
    if fraction >= USAGE_CRITICAL_FRACTION {
        UsageLevel::Critical
    } else if fraction >= USAGE_WARN_FRACTION {
        UsageLevel::Warn
    } else {
        UsageLevel::Normal
    }
}

/// What a usage meter is filled with — the fill exactly, alpha included.
///
/// **Colour appears on a meter only when it means something** (gh#178). Normal
/// usage was painted with the accent, which is the review hue: on a page of
/// four accounts, three of them fine, the eye was pulled to twelve indigo bars
/// saying nothing, and the amber one that meant "you are nearly out" had to
/// compete with them. Below the warning threshold a bar is a neutral rule; the
/// ramp's hues start at 80% and mean the two things they mean everywhere else.
pub fn usage_color(level: UsageLevel, theme: &Theme) -> Hsla {
    match level {
        UsageLevel::Normal => theme.white_alpha(0.30),
        UsageLevel::Warn => theme.warning.opacity(0.85),
        UsageLevel::Critical => theme.danger.opacity(0.85),
    }
}

// ---------------------------------------------------------------------------
// Pure: what a plan costs (gh#178)
// ---------------------------------------------------------------------------

/// The `[account."…"]` key a login's plan is written under: the email it signs
/// in as, else its slot id.
///
/// The email first because that is what the board *records*: an attempt stores
/// whose subscription it spent, and a slot id never appears in that column
/// (`comet_board::prices`). A login with no address falls back to the slot,
/// which the same lookup accepts.
pub fn plan_slot(account: &AgentAccount) -> String {
    account
        .email
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .unwrap_or(account.id.as_str())
        .to_string()
}

/// The plan configured for a login, under whichever spelling `routing.toml`
/// used — the same either-name rule the board prices with, so a page cannot
/// show "no plan" for a subscription the stats page is already crediting.
pub fn plan_for<'a>(
    plans: &'a BTreeMap<String, AccountConfig>,
    account: &AgentAccount,
) -> Option<&'a AccountConfig> {
    let email = account
        .email
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty());
    let names_it = |candidate: &str| {
        let candidate = candidate.trim();
        !candidate.is_empty()
            && (candidate.eq_ignore_ascii_case(&account.id)
                || email.is_some_and(|e| candidate.eq_ignore_ascii_case(e)))
    };
    plans.iter().find_map(|(slot, cfg)| {
        (names_it(slot) || cfg.email.as_deref().is_some_and(names_it)).then_some(cfg)
    })
}

/// The plan line on an account row: what it costs, in the operator's words
/// where they gave any, and the invitation to say so where they did not.
pub fn plan_summary(plan: Option<&AccountConfig>) -> String {
    let Some(plan) = plan else {
        // Never "$0" and never "free": comet cannot see a bill, and a figure
        // nobody entered is unknown (`comet_proto::view::rates`).
        return "Add what this plan costs".to_string();
    };
    let cost = format!("{}/mo", human_usd(plan.monthly_usd));
    match plan
        .plan
        .as_deref()
        .map(str::trim)
        .filter(|label| !label.is_empty())
    {
        Some(label) => format!("{label} · {cost}"),
        None => cost,
    }
}

/// Why a `ListAgentAccounts` load is happening. Pure input to
/// [`force_usage_for`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadTrigger {
    /// Page construction — the visit's first list.
    Mount,
    /// "Click to retry" after a failed load — still the visit's first
    /// successful list.
    Retry,
    /// The explicit Refresh button.
    Refresh,
    /// After a completed add-account login flow.
    PostLogin,
    /// After Switch/Forget succeeds.
    PostAction,
}

/// Whether a load should ask the engine to probe usage (`forceUsage`). The
/// engine only hits the provider when forced; non-forced lists serve the 60s
/// usage cache or nothing (engine/src/agent_accounts.rs module docs — the
/// design expects the UI to force "on page mount/refresh"). The visit's first
/// list (mount, or retry after a failure) must force, or every first open
/// renders "Usage unavailable" until a manual Refresh — the old app fetched
/// usage on every list. Post-Switch/Forget lists ride the still-warm cache.
pub fn force_usage_for(trigger: LoadTrigger) -> bool {
    match trigger {
        LoadTrigger::Mount | LoadTrigger::Retry | LoadTrigger::Refresh | LoadTrigger::PostLogin => {
            true
        }
        LoadTrigger::PostAction => false,
    }
}

/// Compact absolute reset moment (comet settings.agents.tsx `formatReset`):
/// a local clock time ("3:45 PM") when it lands within ~22h, else a short
/// weekday ("Mon"); the caller prefixes "resets ". Pure given `now`.
pub fn format_reset(resets_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<String> {
    use chrono::Local;
    let at = resets_at?;
    let local = at.with_timezone(&Local);
    Some(if at.signed_duration_since(now).num_hours() < 22 {
        format!("resets {}", local.format("%-I:%M %p"))
    } else {
        format!("resets {}", local.format("%a"))
    })
}

/// The provider cards, in display order: (harness, name, CLI command — named
/// in the empty-state copy, comet settings.agents.tsx `PROVIDERS`).
pub const PROVIDERS: [(HarnessId, &str, &str); 3] = [
    (HarnessId::ClaudeCode, "Claude Code", "claude"),
    (HarnessId::Codex, "Codex", "codex"),
    // opencode signs its own providers in (`opencode auth login`); comet has no
    // add/swap flow for it, so the card renders the detected login without an
    // "Add account" action.
    (HarnessId::Opencode, "OpenCode", "opencode auth login"),
];

/// Whether comet drives the provider's login flow (claude/codex do; opencode
/// manages its own provider auth).
fn supports_login(harness: HarnessId) -> bool {
    matches!(harness, HarnessId::ClaudeCode | HarnessId::Codex)
}

/// Accounts of one provider, active first (stable otherwise). Pure.
pub fn provider_accounts(
    snapshot: &AgentAccountsSnapshot,
    harness: HarnessId,
) -> Vec<&AgentAccount> {
    let mut accounts: Vec<&AgentAccount> = snapshot
        .accounts
        .iter()
        .filter(|a| a.harness == harness)
        .collect();
    accounts.sort_by_key(|a| !a.active);
    accounts
}

// ---------------------------------------------------------------------------
// Entity
// ---------------------------------------------------------------------------

enum LoginFlow {
    /// StartAgentLogin in flight.
    Starting { harness: HarnessId },
    /// Claude-style: open the URL, paste the code back.
    PasteCode {
        harness: HarnessId,
        start: AgentLoginStart,
        submitting: bool,
        error: Option<SharedString>,
    },
    /// Codex-style: open the URL, poll until the browser flow lands.
    Browser {
        harness: HarnessId,
        start: AgentLoginStart,
        message: Option<SharedString>,
        error: Option<SharedString>,
    },
    /// Codex on another device (gh#193): show the one-time code to enter at the
    /// URL — on whatever device has a browser, which is this one — while the
    /// remote CLI polls OpenAI. Same poll loop as `Browser`; different story,
    /// because the code goes *out* rather than coming back.
    DeviceCode {
        harness: HarnessId,
        start: AgentLoginStart,
        message: Option<SharedString>,
        error: Option<SharedString>,
    },
}

impl LoginFlow {
    /// Dialog title (comet: "Add Claude account" / "Add Codex account").
    fn title(&self) -> &'static str {
        let harness = match self {
            LoginFlow::Starting { harness }
            | LoginFlow::PasteCode { harness, .. }
            | LoginFlow::Browser { harness, .. }
            | LoginFlow::DeviceCode { harness, .. } => *harness,
        };
        match harness {
            HarnessId::Codex => "Add Codex account",
            HarnessId::Opencode => "OpenCode sign-in",
            _ => "Add Claude account",
        }
    }
}

/// The one field this page wants out of `ReadBoardConfig` / `WriteBoardConfig`
/// — the parse, for its `[account."…"]` tables.
///
/// A narrow reading of the same reply the Routing page takes whole
/// (`comet_board::routes::RoutingView`). Narrow on purpose: a `routing.toml`
/// that does not parse comes back with no `config` at all, and this page should
/// keep working — showing no plans, because there are none to show — rather
/// than failing to read a reply whose broken half is somebody else's screen.
#[derive(Debug, Clone, Deserialize)]
struct BoardConfigReply {
    routing: BoardRouting,
}

#[derive(Debug, Clone, Deserialize)]
struct BoardRouting {
    #[serde(default)]
    config: Option<RoutingConfig>,
}

/// The plans this page can show and set: which device hosts the board, and
/// what its `routing.toml` says each subscription costs.
///
/// Absent until a board answers. A device with no board reachable shows no
/// plan affordance at all rather than a field whose writes would fail — the
/// laptop half of a two-machine setup is a normal thing to be sitting at.
#[derive(Debug, Default)]
struct Plans {
    /// The board host; `None` is this device (or, before `loaded`, unknown).
    host: Option<String>,
    /// True once a board answered — the gate on the whole affordance.
    reachable: bool,
    /// `[account."<slot>"]`, keyed as the file keys it.
    entries: BTreeMap<String, AccountConfig>,
    /// The slot a write is in flight for.
    busy: Option<String>,
}

/// The "what does this plan cost" field, open over one account row.
struct PlanDialog {
    /// The `[account."…"]` key being written — an email, or a slot id.
    slot: String,
    /// The login as the row names it, for the dialog's copy.
    account: SharedString,
    input: Entity<ComposerInput>,
    error: Option<SharedString>,
    _events: Subscription,
}

pub struct AccountsPage {
    state: Entity<AppState>,
    /// Which device's logins are shown; `None` = this device (no passthrough).
    /// Retargeted by the page-header device switcher (comet parity: the
    /// accounts RPCs are relay-forwardable, CLI logins are per-device).
    target_device: Option<String>,
    device_menu_open: bool,
    /// Outside-click dismissal instant — suppresses the trigger click that
    /// follows the same mouse-down from instantly reopening the menu.
    device_menu_dismissed_at: Option<std::time::Instant>,
    snapshot: Loadable<AgentAccountsSnapshot>,
    /// Account id with an in-flight Switch/Forget.
    busy_account: Option<String>,
    login: Option<LoginFlow>,
    /// What each login's subscription costs (gh#178) — read from, and written
    /// to, the board's `routing.toml`.
    plans: Plans,
    plan_dialog: Option<PlanDialog>,
    error: Option<SharedString>,
    code_input: Entity<ComposerInput>,
    load_task: Option<Task<()>>,
    action_task: Option<Task<()>>,
    poll_task: Option<Task<()>>,
    /// The board-config read/write runs on its own slot: dropping a gpui `Task`
    /// cancels it, and an accounts refresh must not cancel a plan write.
    plan_task: Option<Task<()>>,
    _observe: Subscription,
    _code_events: Subscription,
}

impl AccountsPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        // The plan read needs an engine, which a page opened during boot may
        // not have yet — so the state observer is also the retry: one more
        // attempt the moment a connection appears, and none after that (the
        // sweep keeps its task handle whether or not it found a board).
        let observe = cx.observe(&state, |page: &mut Self, _, cx| {
            if page.plan_task.is_none() {
                page.load_plans(cx);
            }
            cx.notify();
        });
        let code_input = cx.new(|cx| ComposerInput::new("Paste the authorization code", cx));
        let code_events = cx.subscribe(&code_input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_code(cx);
            }
        });
        let mut page = Self {
            state,
            target_device: None,
            device_menu_open: false,
            device_menu_dismissed_at: None,
            snapshot: Loadable::Idle,
            busy_account: None,
            login: None,
            plans: Plans::default(),
            plan_dialog: None,
            error: None,
            code_input,
            load_task: None,
            action_task: None,
            poll_task: None,
            plan_task: None,
            _observe: observe,
            _code_events: code_events,
        };
        // Force the usage probe on the visit's first list — a plain list
        // returns no usage windows on a cold engine cache, which rendered
        // every account as "Usage unavailable" until a manual Refresh. The
        // Loading skeleton (meter ghosts) covers the probe latency, so
        // "Usage unavailable" is reserved for a probe that genuinely failed.
        page.load(force_usage_for(LoadTrigger::Mount), cx);
        page.load_plans(cx);
        page
    }

    // ---- what each subscription costs (gh#178) ----

    /// Read the board's `routing.toml` for `[account."…"]`, sweeping for the
    /// device that hosts a board.
    ///
    /// Same contract the Routing page and the board panel sweep on: the engine
    /// refuses every board method when it hosts no board, so a candidate that
    /// errors has answered "not me". A sweep that finds nobody is not an error
    /// on this page — the logins are still the page's subject, and the plan
    /// affordance simply does not appear.
    fn load_plans(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let (devices, local) = {
            let state = self.state.read(cx);
            (state.devices.clone(), state.local_device_id.clone())
        };
        let candidates = board::host_candidates(&devices, local.as_deref());
        self.plan_task = Some(cx.spawn(async move |this, cx| {
            for candidate in candidates {
                let mut params = serde_json::json!({});
                if let (Some(host), Some(object)) = (candidate.as_deref(), params.as_object_mut()) {
                    object.insert("targetDeviceId".into(), serde_json::json!(host));
                }
                let Ok(value) = engine
                    .client()
                    .call(methods::READ_BOARD_CONFIG, params)
                    .await
                else {
                    continue;
                };
                let Ok(reply) = serde_json::from_value::<BoardConfigReply>(value) else {
                    continue;
                };
                this.update(cx, |page, cx| {
                    page.plans.host = candidate;
                    page.adopt_config(reply);
                    cx.notify();
                })
                .ok();
                return;
            }
        }));
    }

    /// Take the plans out of a board-config reply. A file that does not parse
    /// has no plans to show and is the Routing page's problem to report — this
    /// page does not get a second opinion on it.
    fn adopt_config(&mut self, reply: BoardConfigReply) {
        self.plans.reachable = true;
        self.plans.entries = reply
            .routing
            .config
            .map(|cfg| cfg.accounts)
            .unwrap_or_default();
    }

    /// Open the plan field over one account row.
    fn open_plan_dialog(&mut self, account: &AgentAccount, cx: &mut Context<Self>) {
        let slot = plan_slot(account);
        let label: SharedString = account
            .email
            .clone()
            .or_else(|| account.display_name.clone())
            .unwrap_or_else(|| slot.clone())
            .into();
        let input = cx.new(|cx| ComposerInput::new("What it costs per month", cx));
        if let Some(plan) = plan_for(&self.plans.entries, account)
            && !plan.monthly_usd.is_zero()
        {
            let current = human_usd(plan.monthly_usd)
                .trim_start_matches('$')
                .to_string();
            input.update(cx, |input, cx| input.set_text(current, cx));
        }
        let events = cx.subscribe(&input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_plan(cx);
            }
        });
        self.plan_dialog = Some(PlanDialog {
            slot,
            account: label,
            input,
            error: None,
            _events: events,
        });
        cx.notify();
    }

    /// Write the plan cost, and replace the page's plans with the board's fresh
    /// read of the file it now holds.
    ///
    /// An emptied field is a *removal*, not a zero: nobody's plan is free, and
    /// `monthly_usd = 0` would have the stats page report a fully subsidised
    /// board (`comet_proto::view::rates` — unpriced is never zero).
    fn submit_plan(&mut self, cx: &mut Context<Self>) {
        let Some(dialog) = self.plan_dialog.as_ref() else {
            return;
        };
        let slot = dialog.slot.clone();
        let typed = dialog.input.read(cx).text().trim().to_string();
        let value = (!typed.is_empty()).then_some(typed);
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        let mut params = serde_json::json!({
            "op": "account",
            "slot": slot,
            "key": "monthly_usd",
            "value": value,
        });
        if let (Some(host), Some(object)) = (self.plans.host.as_deref(), params.as_object_mut()) {
            object.insert("targetDeviceId".into(), serde_json::json!(host));
        }
        self.plans.busy = Some(slot);
        cx.notify();
        self.plan_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::WRITE_BOARD_CONFIG, params)
                .await;
            this.update(cx, |page, cx| {
                page.plans.busy = None;
                match result {
                    Ok(value) => match serde_json::from_value::<BoardConfigReply>(value) {
                        Ok(reply) => {
                            page.adopt_config(reply);
                            page.plan_dialog = None;
                        }
                        Err(err) => page.error = Some(format!("Unreadable reply: {err}").into()),
                    },
                    // The board's refusals are written to be read ("`free` is
                    // not an amount in dollars"), and they belong in the dialog
                    // beside the field that produced them.
                    Err(err) => {
                        if let Some(dialog) = page.plan_dialog.as_mut() {
                            dialog.error = Some(format!("{err}").into());
                        } else {
                            page.error = Some(format!("{err}").into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Retarget the page at another device's logins: every accounts RPC is
    /// relay-forwardable, so the whole page — list, usage probes, switch,
    /// forget, login flows — follows the passthrough.
    fn set_target_device(&mut self, target: Option<String>, cx: &mut Context<Self>) {
        self.device_menu_open = false;
        if self.target_device == target {
            cx.notify();
            return;
        }
        self.target_device = target;
        // A different device = a different accounts world: drop in-flight
        // login/action state and reload with a forced usage probe (the new
        // device's cache is cold).
        self.login = None;
        self.busy_account = None;
        self.error = None;
        self.load(force_usage_for(LoadTrigger::Mount), cx);
    }

    /// Params with the `targetDeviceId` passthrough merged in.
    fn params(&self, value: serde_json::Value) -> serde_json::Value {
        let mut value = value;
        if let (Some(target), Some(object)) = (&self.target_device, value.as_object_mut()) {
            object.insert("targetDeviceId".into(), serde_json::json!(target));
        }
        value
    }

    /// The page-header device switcher (comet device-switcher.tsx): a quiet
    /// trigger — platform glyph · name · presence dot · sort glyph — opening a
    /// dropdown of every registered device. Selecting one retargets the page.
    fn render_device_switcher(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        use crate::icons::{self, icon};
        let (mut devices, local_id) = {
            let s = self.state.read(cx);
            (s.devices.clone(), s.local_device_id.clone())
        };
        // Stable row order (registration time, then id) — comet's switcher
        // sorts the same way so rows never reshuffle on heartbeats.
        devices.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let effective = self.target_device.clone().or_else(|| local_id.clone());
        let selected = devices
            .iter()
            .find(|d| Some(d.id.as_str()) == effective.as_deref())
            .cloned();
        let platform_glyph = |platform: &str| match platform {
            "macos" | "darwin" => icons::LAPTOP,
            "ios" | "android" => icons::SMARTPHONE,
            _ => icons::MONITOR,
        };
        let trigger_glyph = platform_glyph(
            selected
                .as_ref()
                .map(|d| d.platform.as_str())
                .unwrap_or("macos"),
        );
        let trigger_label: SharedString = selected
            .as_ref()
            .map(|d| d.name.clone().into())
            .unwrap_or_else(|| SharedString::from("This device"));
        let emerald = theme.settled;
        let open = self.device_menu_open;

        let mut trigger = div()
            .id("accounts-device-switcher")
            .flex_none()
            .h(px(28.0))
            .px(px(8.0))
            .rounded(px(Theme::RADIUS_CHIP))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .cursor_pointer()
            .bg(if open {
                theme.white_alpha(0.06)
            } else {
                gpui::transparent_black()
            })
            .when(!open, |el| el.hover(|s| s.bg(theme.white_alpha(0.04))))
            .on_click(cx.listener(|this, _, _, cx| {
                let just_dismissed = this
                    .device_menu_dismissed_at
                    .is_some_and(|at| at.elapsed() < Duration::from_millis(400));
                this.device_menu_open = !this.device_menu_open && !just_dismissed;
                this.device_menu_dismissed_at = None;
                cx.notify();
            }))
            .child(
                icon(trigger_glyph)
                    .size(px(16.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(Theme::TEXT_DENSE))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(trigger_label),
            )
            .child(
                div()
                    .size(px(6.0))
                    // round-ok: status dot
                    .rounded_full()
                    .flex_none()
                    .bg(if effective == local_id {
                        emerald
                    } else {
                        theme.white_alpha(0.2)
                    }),
            )
            .child(
                icon(icons::SORT_VERTICAL)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(if open {
                        theme.text_muted
                    } else {
                        theme.text_subtle
                    }),
            );

        if open {
            let menu = popover::popover_card(theme)
                .w(px(220.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.device_menu_open = false;
                    this.device_menu_dismissed_at = Some(std::time::Instant::now());
                    cx.notify();
                }))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(popover::menu_heading(theme, "Devices"))
                .children(devices.into_iter().enumerate().map(|(ix, d)| {
                    let is_active = Some(d.id.as_str()) == effective.as_deref();
                    let is_local = local_id.as_deref() == Some(d.id.as_str());
                    let glyph = platform_glyph(&d.platform);
                    let name: SharedString = d.name.clone().into();
                    let pick_local = is_local;
                    let pick_id = d.id.clone();
                    popover::menu_row(theme, is_active, format!("accounts-device-row-{ix}"))
                        .id(("accounts-device-row", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            // Local device = no passthrough (calls stay direct).
                            let target = (!pick_local).then(|| pick_id.clone());
                            this.set_target_device(target, cx);
                        }))
                        .child(
                            icon(glyph)
                                .size(px(16.0))
                                .flex_none()
                                .text_color(theme.text_muted),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(name))
                        .when(is_local, |el| {
                            el.child(
                                div()
                                    .flex_none()
                                    .text_size(px(Theme::TEXT_CAPTION))
                                    .text_color(theme.text_subtle)
                                    .child(SharedString::from("You")),
                            )
                        })
                        .when(is_active, |el| el.child(popover::menu_check(theme)))
                        .child(
                            div()
                                .size(px(6.0))
                                // round-ok: status dot
                                .rounded_full()
                                .flex_none()
                                .bg(if is_local {
                                    emerald
                                } else {
                                    theme.white_alpha(0.2)
                                }),
                        )
                }))
                .into_any_element();
            trigger = trigger.child(popover::anchored_menu("accounts-device-menu", menu));
        }
        trigger.into_any_element()
    }

    fn load(&mut self, force_usage: bool, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.snapshot = Loadable::Error("Engine not connected".into());
            return;
        };
        self.snapshot = Loadable::Loading;
        let params = self.params(serde_json::json!({ "forceUsage": force_usage }));
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_AGENT_ACCOUNTS, params)
                .await;
            this.update(cx, |page, cx| {
                page.snapshot = match result {
                    Ok(value) => match serde_json::from_value::<AgentAccountsSnapshot>(value) {
                        Ok(snapshot) => Loadable::Ready(snapshot),
                        Err(err) => Loadable::Error(err.to_string()),
                    },
                    Err(err) => Loadable::Error(err.to_string()),
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Switch / Forget an account.
    fn account_action(
        &mut self,
        method: &'static str,
        account: &AgentAccount,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.busy_account = Some(account.id.clone());
        self.error = None;
        // Tolerant param shape: both `id` and `accountId` plus the harness.
        let params = self.params(serde_json::json!({
            "id": account.id,
            "accountId": account.id,
            "harness": account.harness,
        }));
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(method, params).await;
            this.update(cx, |page, cx| {
                page.busy_account = None;
                match result {
                    Ok(_) => page.load(force_usage_for(LoadTrigger::PostAction), cx),
                    Err(err) => page.error = Some(format!("{err}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    // ---- add-account flows ----

    fn start_login(&mut self, harness: HarnessId, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.login = Some(LoginFlow::Starting { harness });
        self.error = None;
        let params = self.params(serde_json::json!({ "harness": harness }));
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::START_AGENT_LOGIN, params)
                .await;
            this.update(cx, |page, cx| {
                match result.and_then(|value| {
                    serde_json::from_value::<AgentLoginStart>(value)
                        .map_err(|e| comet_rpc::RpcError::Failed(e.to_string()))
                }) {
                    Ok(start) => {
                        cx.open_url(&start.url);
                        match start.mode {
                            AgentLoginMode::PasteCode => {
                                page.code_input
                                    .update(cx, |input, cx| input.set_text("", cx));
                                page.login = Some(LoginFlow::PasteCode {
                                    harness,
                                    start,
                                    submitting: false,
                                    error: None,
                                });
                            }
                            AgentLoginMode::Browser => {
                                page.login = Some(LoginFlow::Browser {
                                    harness,
                                    start,
                                    message: None,
                                    error: None,
                                });
                                page.spawn_poll(cx);
                            }
                            AgentLoginMode::DeviceCode => {
                                page.login = Some(LoginFlow::DeviceCode {
                                    harness,
                                    start,
                                    message: None,
                                    error: None,
                                });
                                page.spawn_poll(cx);
                            }
                        }
                    }
                    Err(err) => {
                        page.login = None;
                        page.error = Some(format!("Login failed to start: {err}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn submit_code(&mut self, cx: &mut Context<Self>) {
        let Some(LoginFlow::PasteCode {
            start, submitting, ..
        }) = &mut self.login
        else {
            return;
        };
        if *submitting {
            return;
        }
        let code = self.code_input.read(cx).text().trim().to_string();
        if code.is_empty() {
            return;
        }
        let login_id = start.login_id.clone();
        *submitting = true;
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.params(serde_json::json!({ "loginId": login_id, "code": code }));
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::COMPLETE_AGENT_LOGIN, params)
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.login = None;
                        page.load(force_usage_for(LoadTrigger::PostLogin), cx);
                    }
                    Err(err) => {
                        if let Some(LoginFlow::PasteCode {
                            submitting, error, ..
                        }) = &mut page.login
                        {
                            *submitting = false;
                            *error = Some(format!("{err}").into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// The wait loop shared by both poll-shaped flows (browser callback and
    /// device code): PollAgentLogin every 1.5s until Done/Error.
    fn spawn_poll(&mut self, cx: &mut Context<Self>) {
        let (Some(LoginFlow::Browser { start, .. }) | Some(LoginFlow::DeviceCode { start, .. })) =
            &self.login
        else {
            return;
        };
        let login_id = start.login_id.clone();
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.params(serde_json::json!({ "loginId": login_id }));
        self.poll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(1500))
                    .await;
                let result = engine
                    .client()
                    .call(methods::POLL_AGENT_LOGIN, params.clone())
                    .await;
                let outcome = this.update(cx, |page, cx| {
                    let (Some(LoginFlow::Browser { message, error, .. })
                    | Some(LoginFlow::DeviceCode { message, error, .. })) = &mut page.login
                    else {
                        return true; // dialog dismissed — stop polling
                    };
                    match result.as_ref().ok().and_then(|value| {
                        serde_json::from_value::<AgentLoginPoll>(value.clone()).ok()
                    }) {
                        Some(poll) => match poll.status {
                            AgentLoginStatus::Done => {
                                page.login = None;
                                page.load(force_usage_for(LoadTrigger::PostLogin), cx);
                                cx.notify();
                                true
                            }
                            AgentLoginStatus::Error => {
                                *error = Some(
                                    poll.message
                                        .unwrap_or_else(|| "Login failed".to_string())
                                        .into(),
                                );
                                cx.notify();
                                true
                            }
                            AgentLoginStatus::Pending => {
                                if let Some(text) = poll.message {
                                    *message = Some(text.into());
                                }
                                cx.notify();
                                false
                            }
                        },
                        None => {
                            let text = match &result {
                                Err(err) => format!("Poll failed: {err}"),
                                Ok(_) => "Poll failed: malformed reply".to_string(),
                            };
                            *error = Some(text.into());
                            cx.notify();
                            true
                        }
                    }
                });
                match outcome {
                    Ok(true) | Err(_) => break,
                    Ok(false) => {}
                }
            }
        }));
    }

    fn cancel_login(&mut self, cx: &mut Context<Self>) {
        let login_id = match &self.login {
            Some(LoginFlow::PasteCode { start, .. })
            | Some(LoginFlow::Browser { start, .. })
            | Some(LoginFlow::DeviceCode { start, .. }) => Some(start.login_id.clone()),
            _ => None,
        };
        self.login = None;
        self.poll_task = None;
        if let (Some(login_id), Some(engine)) = (login_id, self.state.read(cx).engine().cloned()) {
            let params = self.params(serde_json::json!({ "loginId": login_id }));
            self.action_task = Some(cx.spawn(async move |_, _| {
                if let Err(err) = engine
                    .client()
                    .call(methods::CANCEL_AGENT_LOGIN, params)
                    .await
                {
                    tracing::debug!(error = %err, "CancelAgentLogin failed (best-effort)");
                }
            }));
        }
        cx.notify();
    }

    // ---- render pieces ----

    /// One usage window (comet settings.agents.tsx `UsageMeter`): label ·
    /// 5px rounded-full bar (indigo → amber ≥80% → red ≥95%) · "NN% used" ·
    /// quiet reset time.
    fn render_usage_meter(
        &self,
        window: &comet_proto::AgentUsageWindow,
        theme: &Theme,
        now: DateTime<Utc>,
    ) -> AnyElement {
        let fraction = window.used_fraction.clamp(0.0, 1.0);
        let level = usage_level(fraction);
        let fill = usage_color(level, theme);
        let reset = format_reset(window.resets_at, now);
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .text_size(px(Theme::TEXT_CAPTION))
            .text_color(theme.text_subtle)
            .child(
                div()
                    .w(px(48.0))
                    .flex_none()
                    .truncate()
                    .child(SharedString::from(window.label.clone())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(56.0))
                    .max_w(px(230.0))
                    .h(px(5.0))
                    // round-ok: usage bar — a 5px bar with round caps, not a box
                    .rounded_full()
                    .overflow_hidden()
                    .bg(theme.white_alpha(0.07))
                    .when(fraction > 0.0, |el| {
                        el.child(
                            div()
                                .h_full()
                                // A 1.5% floor keeps tiny non-zero usage
                                // visible (comet `max(used, 1.5)%`).
                                .w(gpui::relative(fraction.max(0.015)))
                                // round-ok: the bar fill, capped to match its track
                                .rounded_full()
                                .bg(fill),
                        )
                    }),
            )
            .child(
                div()
                    .w(px(64.0))
                    .flex_none()
                    .text_right()
                    .child(SharedString::from(format!(
                        "{}% used",
                        (fraction * 100.0).round() as u32
                    ))),
            )
            .when_some(reset, |el, reset| {
                el.child(
                    div()
                        .flex_none()
                        .truncate()
                        .child(SharedString::from(reset)),
                )
            })
            .into_any_element()
    }

    /// One account row (comet settings.agents.tsx `AccountRow`): initial
    /// avatar, email + usage meters left; badges over the Switch/Forget
    /// actions right-anchored.
    fn render_account_row(
        &self,
        account: &AgentAccount,
        ix: usize,
        first: bool,
        theme: &Theme,
        now: DateTime<Utc>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::settings::widgets;
        let is_busy = self.busy_account.as_deref() == Some(account.id.as_str());
        let email: SharedString = account
            .email
            .clone()
            .or_else(|| account.display_name.clone())
            .unwrap_or_else(|| "Unknown account".into())
            .into();
        let initial: SharedString = email
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "?".into())
            .into();
        let switch_account = account.clone();
        let forget_account = account.clone();

        let badges = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .when(account.active, |el| {
                el.child(widgets::badge_active(theme, "Active"))
            })
            .when_some(account.plan_label.clone(), |el, plan| {
                el.child(widgets::badge(theme, plan))
            });

        // What this subscription costs (gh#178), under the meters that spend
        // it. Only where a board can be reached: it is that board's
        // `routing.toml` the number lands in, and a field whose write has
        // nowhere to go is worse than no field.
        let plan = plan_for(&self.plans.entries, account);
        let plan_writing = self.plans.busy.as_deref() == Some(plan_slot(account).as_str());
        let plan_row: Option<AnyElement> = self.plans.reachable.then(|| {
            let account = account.clone();
            widgets::ghost_action(theme)
                .id(("account-plan", ix))
                .self_start()
                .mt(px(6.0))
                .px(px(6.0))
                .py(px(2.0))
                .text_size(px(Theme::TEXT_DENSE))
                .text_color(if plan.is_some() {
                    theme.text_subtle
                } else {
                    theme.text_faint
                })
                .when(plan_writing, |el| el.opacity(0.5))
                .hover(widgets::ghost_hover(theme))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.open_plan_dialog(&account, cx);
                }))
                .child(SharedString::from(plan_summary(plan)))
                .into_any_element()
        });

        // Actions only on INACTIVE accounts (comet `{!account.active && …}`):
        // an icon-only Forget (trash, hover → foreground) then Switch, which
        // reads "Switching…" while the activate round-trips.
        let actions: Option<gpui::Div> = (!account.active).then(|| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .child(
                    div()
                        .id(("account-forget", ix))
                        .rounded(px(Theme::RADIUS_CHIP))
                        .px(px(6.0))
                        .py(px(4.0))
                        .text_color(theme.text_muted)
                        .cursor_pointer()
                        .when(is_busy, |el| el.opacity(0.5))
                        .hover(|s| s.bg(theme.white_alpha(0.06)).text_color(theme.text))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.account_action(methods::FORGET_AGENT_ACCOUNT, &forget_account, cx);
                        }))
                        .child(
                            crate::icons::icon(crate::icons::TRASH_BIN_MINIMALISTIC)
                                .size(px(14.0))
                                .text_color(theme.text_muted),
                        ),
                )
                .when(account.switchable, |el| {
                    el.child(
                        crate::popover::btn_primary(
                            theme,
                            if is_busy { "Switching…" } else { "Switch" },
                        )
                        .id(("account-switch", ix))
                        .px(px(8.0))
                        .py(px(4.0))
                        .rounded(px(Theme::RADIUS_CHIP))
                        .text_size(px(Theme::TEXT_CAPTION))
                        .when(is_busy, |el| el.opacity(0.5))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.account_action(
                                methods::ACTIVATE_AGENT_ACCOUNT,
                                &switch_account,
                                cx,
                            );
                        })),
                    )
                })
        });

        div()
            .px(px(20.0))
            .py(px(14.0))
            .when(!first, |el| el.border_t_1().border_color(theme.border))
            .flex()
            .flex_row()
            .items_stretch()
            .gap(px(12.0))
            .child(
                // Initial avatar: size-8 rounded-full border bg-white/[0.03].
                div()
                    .flex_none()
                    .self_center()
                    .size(px(32.0))
                    .rounded(px(Theme::RADIUS_ROW))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.white_alpha(0.03))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(Theme::TEXT_DENSE))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child(initial),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(widgets::row_title(theme, email))
                    .map(|el| {
                        // Meters XOR the quiet fallback line — never both
                        // (comet: `usage ? meters : "Usage unavailable"…`).
                        if account.usage_windows.is_empty() {
                            el.child(
                                div()
                                    .mt(px(6.0))
                                    .truncate()
                                    .text_size(px(Theme::TEXT_CAPTION))
                                    .text_color(theme.text_subtle)
                                    .child(SharedString::from(if account.switchable {
                                        "Usage unavailable"
                                    } else {
                                        "Credentials unavailable"
                                    })),
                            )
                        } else {
                            el.child(
                                div().mt(px(6.0)).flex().flex_col().gap(px(4.0)).children(
                                    account
                                        .usage_windows
                                        .iter()
                                        .map(|w| self.render_usage_meter(w, theme, now)),
                                ),
                            )
                        }
                    })
                    .children(plan_row),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .items_end()
                    .justify_between()
                    .gap(px(8.0))
                    .child(badges)
                    .children(actions),
            )
            .into_any_element()
    }

    fn render_login_dialog(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        // The dialog's failure line, in the tone that inverts for light — the
        // literal red-300 it held was invisible on a light dialog (gh#178).
        let red_text = theme.danger_text();
        let login = self.login.as_ref()?;
        let title = login.title();
        let url_link =
            |id: &'static str, label: &'static str, url: &str, cx: &mut Context<Self>| {
                let open_url = url.to_string();
                // "Reopen the …" text link (comet: `text-[12px] hover:underline`).
                div()
                    .id(id)
                    .mt(px(6.0))
                    .text_size(px(Theme::TEXT_DENSE))
                    .text_color(theme.text_subtle)
                    .truncate()
                    .cursor_pointer()
                    .hover(|s| s.text_color(theme.text))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.open_url(&open_url);
                    }))
                    .child(SharedString::from(label))
            };
        let body: AnyElement = match login {
            LoginFlow::Starting { .. } => div()
                .mt(px(8.0))
                .child(popover::skeleton_rows("login-starting", &theme, 2))
                .into_any_element(),
            LoginFlow::PasteCode {
                start,
                submitting,
                error,
                ..
            } => {
                let submitting = *submitting;
                div()
                    .flex()
                    .flex_col()
                    .child(div().mt(px(8.0)).child(popover::dialog_body(
                        &theme,
                        "A browser window opened. Sign in to the account you want to add, \
                         approve access, then paste the code Anthropic shows you below. Your \
                         current login is untouched until you switch.",
                    )))
                    .child(url_link(
                        "login-open-url",
                        "Reopen the authorization page",
                        &start.url,
                        cx,
                    ))
                    .child(
                        div().mt(px(12.0)).child(
                            popover::dialog_field(
                                &theme,
                                self.code_input.clone().into_any_element(),
                            )
                            .font_family(theme.font_mono.clone())
                            .text_size(px(Theme::TEXT_BODY)),
                        ),
                    )
                    .when_some(error.clone(), |el, message| {
                        el.child(
                            div()
                                .mt(px(8.0))
                                .text_size(px(Theme::TEXT_DENSE))
                                .text_color(red_text)
                                .child(message),
                        )
                    })
                    .child(
                        div()
                            .mt(px(16.0))
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap(px(8.0))
                            .child(
                                popover::btn_ghost(&theme, "Cancel", "login-cancel")
                                    .id("login-cancel")
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel_login(cx))),
                            )
                            .child(
                                popover::btn_primary(
                                    &theme,
                                    if submitting {
                                        "Verifying…"
                                    } else {
                                        "Add account"
                                    },
                                )
                                .id("login-submit-code")
                                .when(submitting, |el| el.opacity(0.5))
                                .on_click(cx.listener(|this, _, _, cx| this.submit_code(cx))),
                            ),
                    )
                    .into_any_element()
            }
            // The two poll-shaped flows share everything below the code chip —
            // spinner, error line, lone Cancel — and differ only in what the
            // operator is being asked to do with a browser.
            LoginFlow::Browser {
                start,
                message,
                error,
                ..
            }
            | LoginFlow::DeviceCode {
                start,
                message,
                error,
                ..
            } => {
                let has_error = error.is_some();
                let user_code: Option<SharedString> = matches!(login, LoginFlow::DeviceCode { .. })
                    .then(|| start.user_code.clone())
                    .flatten()
                    .map(SharedString::from);
                let device_code = user_code.is_some();
                div()
                    .flex()
                    .flex_col()
                    .child(div().mt(px(8.0)).child(popover::dialog_body(
                        &theme,
                        if device_code {
                            "That device has no browser to finish in, so it is waiting on \
                             OpenAI for a code instead. Sign in here to the account you want \
                             to add, enter the code below, and leave this open. The new login \
                             is captured in an isolated profile over there — its current \
                             session is untouched until you switch."
                        } else {
                            "Finish signing in to OpenAI in your browser. The new login is \
                             captured in an isolated profile — your current session is \
                             untouched until you switch."
                        },
                    )))
                    .child(url_link(
                        "login-open-url-browser",
                        if device_code {
                            "Open the OpenAI device page"
                        } else {
                            "Reopen the sign-in page"
                        },
                        &start.url,
                        cx,
                    ))
                    .when_some(user_code, |el, code| {
                        // The payload of the whole dialog: big and monospaced,
                        // because it is read off this screen and typed by hand.
                        // No hair-space tracking (the menu-heading trick) —
                        // this is a string someone transcribes character for
                        // character, and nothing invisible belongs in it.
                        el.child(
                            div()
                                .mt(px(14.0))
                                .px(px(14.0))
                                .py(px(10.0))
                                .rounded(px(Theme::RADIUS_ROW))
                                .bg(theme.white_alpha(0.05))
                                .flex()
                                .flex_row()
                                .justify_center()
                                .child(
                                    div()
                                        .font_family(theme.font_mono.clone())
                                        .text_size(px(Theme::TEXT_FIGURE))
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .text_color(theme.text)
                                        .child(code),
                                ),
                        )
                    })
                    .when(!has_error, |el| {
                        el.child(
                            div()
                                .mt(px(16.0))
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.0))
                                .child(crate::loaders::gradient_spinner("login-poll", &theme, 3.0))
                                .child(
                                    div()
                                        .text_size(px(Theme::TEXT_DENSE))
                                        .text_color(theme.text_muted)
                                        .child(message.clone().unwrap_or_else(|| {
                                            SharedString::from(if device_code {
                                                "Waiting for the code to be entered…"
                                            } else {
                                                "Waiting for the browser…"
                                            })
                                        })),
                                ),
                        )
                    })
                    .when_some(error.clone(), |el, message| {
                        el.child(
                            div()
                                .mt(px(12.0))
                                .text_size(px(Theme::TEXT_DENSE))
                                .text_color(red_text)
                                .child(message),
                        )
                    })
                    .child(
                        div().mt(px(16.0)).flex().flex_row().justify_end().child(
                            popover::btn_ghost(
                                &theme,
                                if has_error { "Close" } else { "Cancel" },
                                "login-cancel",
                            )
                            .id("login-cancel")
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_login(cx))),
                        ),
                    )
                    .into_any_element()
            }
        };
        let card = popover::dialog_card(&theme)
            .child(popover::dialog_title(&theme, title))
            .child(body)
            .into_any_element();
        Some(popover::modal("add-account-dialog", viewport, card))
    }

    /// The plan field (gh#178): one number, and the sentence that says why
    /// comet is asking for it rather than reading it.
    fn render_plan_dialog(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let dialog = self.plan_dialog.as_ref()?;
        let busy = self.plans.busy.as_deref() == Some(dialog.slot.as_str());
        let account = dialog.account.clone();
        let error = dialog.error.clone();
        let card = popover::dialog_card(&theme)
            .child(popover::dialog_title(&theme, "What this plan costs"))
            .child(div().mt(px(8.0)).child(popover::dialog_body(
                &theme,
                format!(
                    "Per month, in dollars, for {account}. Comet never sees your bill — \
                         this is the one number it cannot find out, and Board stats reads it \
                         against what the same work would have cost at list price."
                ),
            )))
            .child(div().mt(px(12.0)).child(popover::dialog_field(
                &theme,
                self.plan_dialog.as_ref()?.input.clone().into_any_element(),
            )))
            .child(
                div()
                    .mt(px(6.0))
                    .text_size(px(Theme::TEXT_DENSE))
                    .text_color(theme.text_subtle)
                    .child(SharedString::from(
                        "Empty: no plan recorded. It lands in the board's routing.toml.",
                    )),
            )
            .when_some(error, |el, message| {
                el.child(
                    div()
                        .mt(px(8.0))
                        .text_size(px(Theme::TEXT_DENSE))
                        .text_color(theme.danger_text())
                        .child(message),
                )
            })
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(&theme, "Cancel", "plan-cancel")
                            .id("plan-cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.plan_dialog = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        popover::btn_primary(&theme, if busy { "Saving…" } else { "Save" })
                            .id("plan-save")
                            .when(busy, |el| el.opacity(0.5))
                            .on_click(cx.listener(|this, _, _, cx| this.submit_plan(cx))),
                    ),
            )
            .into_any_element();
        Some(popover::modal("account-plan-dialog", viewport, card))
    }

    /// A ghost account row (comet settings.agents.tsx `SkeletonRow`): avatar,
    /// email line, two usage-meter ghosts, a badge — same geometry as the real
    /// row so loaded data lands without a layout jump. `dim` fades row two.
    fn render_skeleton_row(
        &self,
        id: (&'static str, usize),
        dim: bool,
        first: bool,
        theme: &Theme,
    ) -> AnyElement {
        use crate::motion::{self, AnimationExt as _};
        // One shape for every placeholder — the badge it stands in for stopped
        // being a full-round pill when the radii closed (gh#174).
        let ghost = |w: gpui::Length, h: f32| {
            div()
                .w(w)
                .h(px(h))
                .flex_none()
                .rounded(px(Theme::RADIUS_CHIP))
                .bg(theme.white_alpha(0.05))
        };
        let meters = div()
            .mt(px(8.0))
            .flex()
            .flex_col()
            .gap(px(7.0))
            .children((0..2).map(|_| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(ghost(px(48.0).into(), 9.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(56.0))
                            .max_w(px(230.0))
                            .h(px(5.0))
                            // round-ok: the usage-bar placeholder, capped like the bar
                            .rounded_full()
                            .bg(theme.white_alpha(0.04)),
                    )
                    .child(ghost(px(64.0).into(), 9.0))
            }));
        let inner = div()
            .flex()
            .flex_row()
            .items_stretch()
            .gap(px(12.0))
            .child(
                div()
                    .flex_none()
                    .self_center()
                    .size(px(32.0))
                    .rounded(px(Theme::RADIUS_ROW))
                    .bg(theme.white_alpha(0.05)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(ghost(px(176.0).into(), 13.0).max_w(gpui::relative(0.6)))
                    .child(meters),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .items_end()
                    .child(ghost(px(64.0).into(), 21.0)),
            );
        div()
            .px(px(20.0))
            .py(px(14.0))
            .when(!first, |el| el.border_t_1().border_color(theme.border))
            .when(dim, |el| el.opacity(0.6))
            .child(
                inner.with_animation(id, motion::COMET_PULSE.repeating(), move |el, delta| {
                    el.opacity(0.55 + 0.35 * motion::pulse_wave(delta))
                }),
            )
            .into_any_element()
    }
}

impl Render for AccountsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::settings::widgets;
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        let dialog = self
            .render_login_dialog(window.viewport_size(), cx)
            .or_else(|| self.render_plan_dialog(window.viewport_size(), cx));
        let refreshing = matches!(self.snapshot, Loadable::Loading);
        let account_count = self
            .snapshot
            .ready()
            .map(|s| s.accounts.len())
            .filter(|&n| n > 0);

        let provider_icon = |harness: HarnessId| match harness {
            HarnessId::Codex => (crate::icons::OPENAI_MARK, None),
            HarnessId::Cursor => (crate::icons::CURSOR_MARK, None),
            HarnessId::Opencode => (crate::icons::OPENCODE_MARK, None),
            _ => (
                crate::icons::CLAUDE_MARK,
                Some(crate::icons::claude_brand()),
            ),
        };
        // Brand mark inside a 24px centered box (comet: `grid size-6
        // place-items-center [&_svg]:size-4`).
        let provider_mark = |harness: HarnessId, theme: &Theme| {
            let (mark, tint) = provider_icon(harness);
            div()
                .flex_none()
                .size(px(24.0))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    crate::icons::icon(mark)
                        .size(px(16.0))
                        .text_color(tint.unwrap_or(theme.text_muted)),
                )
        };

        // One section per provider (comet settings.agents.tsx `ProviderSection`):
        // brand header + Add account, then the account rows card.
        let sections: Vec<AnyElement> = match &self.snapshot {
            Loadable::Idle | Loadable::Loading => PROVIDERS
                .into_iter()
                .map(|(harness, name, _cli)| {
                    let skeleton_id = match harness {
                        HarnessId::Codex => "accounts-skeleton-codex",
                        HarnessId::Opencode => "accounts-skeleton-opencode",
                        _ => "accounts-skeleton-claude",
                    };
                    div()
                        .mt(px(24.0))
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.0))
                                .child(provider_mark(harness, &theme))
                                .child(
                                    div()
                                        .text_size(px(Theme::TEXT_BODY))
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .text_color(theme.text)
                                        .child(SharedString::from(name)),
                                ),
                        )
                        .child(
                            // Ghost rows shaped like real ones (row two dimmed)
                            // so the card keeps its size while data develops.
                            widgets::section_card(&theme)
                                .mt(px(8.0))
                                .child(self.render_skeleton_row(
                                    (skeleton_id, 0),
                                    false,
                                    true,
                                    &theme,
                                ))
                                .child(self.render_skeleton_row(
                                    (skeleton_id, 1),
                                    true,
                                    false,
                                    &theme,
                                )),
                        )
                        .into_any_element()
                })
                .collect(),
            Loadable::Error(message) => {
                let message = message.clone();
                vec![
                    widgets::error_strip(&theme, message)
                        .id("accounts-load-error")
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            // Retry IS the visit's first successful list — force usage.
                            this.load(force_usage_for(LoadTrigger::Retry), cx)
                        }))
                        .child(
                            div()
                                .mt(px(4.0))
                                .text_size(px(Theme::TEXT_CAPTION))
                                .text_color(theme.text_muted)
                                .child(SharedString::from("Click to retry")),
                        )
                        .into_any_element(),
                ]
            }
            Loadable::Ready(snapshot) => {
                let snapshot = snapshot.clone();
                PROVIDERS
                    .into_iter()
                    .map(|(harness, name, cli)| {
                        let accounts = provider_accounts(&snapshot, harness);
                        // EVERY warning renders its own strip (comet maps them).
                        let warnings: Vec<String> = snapshot
                            .warnings
                            .iter()
                            .filter(|w| w.harness == harness)
                            .map(|w| w.message.clone())
                            .collect();
                        let rows: Vec<AnyElement> = accounts
                            .iter()
                            .enumerate()
                            .map(|(ix, account)| {
                                self.render_account_row(account, ix, ix == 0, &theme, now, cx)
                            })
                            .collect();
                        let has_rows = !rows.is_empty();
                        let add_id: SharedString = format!("add-account-{name}").into();
                        let card = widgets::section_card(&theme).mt(px(8.0));
                        let card = if !has_rows {
                            card.child(
                                div()
                                    .px(px(20.0))
                                    .py(px(32.0))
                                    .text_center()
                                    .text_size(px(Theme::TEXT_BODY))
                                    .text_color(theme.text_subtle)
                                    .child(SharedString::from(if supports_login(harness) {
                                        format!(
                                            "No {name} login detected on this device — sign in \
                                             with \u{201C}{cli}\u{201D} or add an account."
                                        )
                                    } else {
                                        format!(
                                            "No {name} login detected on this device — sign in \
                                             with \u{201C}{cli}\u{201D}."
                                        )
                                    })),
                            )
                        } else {
                            card.children(rows)
                        };
                        let add_action = if supports_login(harness) {
                            widgets::ghost_action(&theme)
                                .id(add_id)
                                .hover(widgets::ghost_hover(&theme))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.start_login(harness, cx);
                                }))
                                .child(
                                    crate::icons::icon(crate::icons::ADD_CIRCLE)
                                        .size(px(16.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(SharedString::from("Add account"))
                                .into_any_element()
                        } else if has_rows {
                            div()
                                .text_size(px(Theme::TEXT_CAPTION))
                                .text_color(theme.text_subtle)
                                .child(SharedString::from("Signed in via opencode"))
                                .into_any_element()
                        } else {
                            // No opencode auth: the empty-state card below
                            // carries the sign-in hint; nothing in the header.
                            div().into_any_element()
                        };
                        div()
                            .mt(px(24.0))
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(8.0))
                                    .child(provider_mark(harness, &theme))
                                    .child(
                                        div()
                                            .text_size(px(Theme::TEXT_BODY))
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .text_color(theme.text)
                                            .child(SharedString::from(name)),
                                    )
                                    .child(div().flex_1())
                                    .child(add_action),
                            )
                            .children(
                                warnings
                                    .into_iter()
                                    .map(|warning| widgets::warning_strip(&theme, warning)),
                            )
                            .child(card)
                            .into_any_element()
                    })
                    .collect()
            }
        };

        div()
            .id("accounts-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(10.0))
                            .child(widgets::page_header(&theme, "Accounts", account_count))
                            .child(div().flex_1())
                            .child(
                                // `text-[12.5px]` + leading 16px Refresh icon,
                                // dimmed while a refresh is in flight (comet
                                // `disabled:opacity-50`).
                                widgets::ghost_action(&theme)
                                    .id("accounts-refresh")
                                    .flex_none()
                                    .text_size(px(Theme::TEXT_DENSE))
                                    .hover(widgets::ghost_hover(&theme))
                                    .when(refreshing, |el| el.opacity(0.5))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.load(force_usage_for(LoadTrigger::Refresh), cx)
                                    }))
                                    .child(
                                        crate::icons::icon(crate::icons::REFRESH)
                                            .size(px(16.0))
                                            .text_color(theme.text_muted),
                                    )
                                    .child(SharedString::from("Refresh")),
                            )
                            .child(self.render_device_switcher(&theme, cx)),
                    )
                    .child(widgets::page_subtitle(
                        &theme,
                        "The Claude Code, Codex and OpenCode logins on this device. Comet \
                         detects the live sessions, keeps each account backed up, and can swap \
                         between them.",
                    ))
                    .when_some(self.error.clone(), |el, message| {
                        el.child(
                            widgets::error_strip(&theme, message)
                                .id("accounts-action-error")
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.error = None;
                                    cx.notify();
                                })),
                        )
                    })
                    .children(sections)
                    // Footer note (comet: `mt-6 text-[12px] leading-relaxed`).
                    .child(
                        div()
                            .mt(px(24.0))
                            .text_size(px(Theme::TEXT_DENSE))
                            .line_height(px(19.0))
                            .text_color(theme.text_subtle)
                            .child(SharedString::from(
                                "Switching rewrites the CLI\u{2019}s stored login, so new \
                                 agent sessions use the selected account immediately. On \
                                 macOS, an already-running Claude Code can hold the previous \
                                 login for up to ~30 seconds (Keychain cache).",
                            )),
                    )
                    // What a plan costs is the one fact here nobody can detect
                    // — say so where the field is, not only inside its dialog.
                    .when(self.plans.reachable, |el| {
                        el.child(
                            div()
                                .mt(px(8.0))
                                .text_size(px(Theme::TEXT_DENSE))
                                .line_height(px(19.0))
                                .text_color(theme.text_subtle)
                                .child(SharedString::from(
                                    "Comet never sees your bill. The plan cost under each \
                                     login is what you tell it, kept in the board\u{2019}s \
                                     routing.toml, and Board stats reads it against the list \
                                     price of the work those logins actually ran.",
                                )),
                        )
                    }),
            )
            .when_some(dialog, |el, dialog| el.child(dialog))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    #[test]
    fn first_load_of_a_visit_forces_the_usage_probe() {
        // The engine only probes usage when forced (M5c); without forcing on
        // mount, the first Accounts open always rendered "Usage unavailable".
        assert!(force_usage_for(LoadTrigger::Mount));
        // A retry after a failed load is still the visit's first successful
        // list — same requirement.
        assert!(force_usage_for(LoadTrigger::Retry));
        // Explicit refresh and a just-completed login always re-probe.
        assert!(force_usage_for(LoadTrigger::Refresh));
        assert!(force_usage_for(LoadTrigger::PostLogin));
        // Switch/Forget re-lists ride the still-warm 60s cache.
        assert!(!force_usage_for(LoadTrigger::PostAction));
    }

    #[test]
    fn usage_thresholds_match_comet() {
        assert_eq!(usage_level(0.0), UsageLevel::Normal);
        assert_eq!(usage_level(0.79), UsageLevel::Normal);
        assert_eq!(usage_level(0.80), UsageLevel::Warn);
        assert_eq!(usage_level(0.94), UsageLevel::Warn);
        assert_eq!(usage_level(0.95), UsageLevel::Critical);
        assert_eq!(usage_level(1.0), UsageLevel::Critical);
    }

    /// A meter spends colour only where the colour means something (gh#178):
    /// neutral below the warning threshold, then the two ramp hues that mean
    /// the same thing everywhere else in the app.
    #[test]
    fn a_normal_meter_spends_no_colour() {
        for theme in [Theme::dark(), Theme::light()] {
            let normal = usage_color(UsageLevel::Normal, &theme);
            assert!(
                !crate::theme::spends_colour(normal),
                "a normal meter is a neutral rule"
            );
            assert_ne!(normal, theme.accent, "normal is not a state");
            assert_eq!(
                usage_color(UsageLevel::Warn, &theme).h,
                theme.warning.h,
                "the warning hue"
            );
            assert_eq!(
                usage_color(UsageLevel::Critical, &theme).h,
                theme.danger.h,
                "the blocked hue"
            );
        }
    }

    #[test]
    fn reset_formatting_is_absolute() {
        use chrono::Local;
        let now = Utc::now();
        assert_eq!(format_reset(None, now), None);
        // Within ~22h: a local clock time ("resets 3:45 PM").
        let soon = now + TimeDelta::minutes(125);
        assert_eq!(
            format_reset(Some(soon), now),
            Some(format!(
                "resets {}",
                soon.with_timezone(&Local).format("%-I:%M %p")
            ))
        );
        // Beyond: a short weekday ("resets Mon").
        let later = now + TimeDelta::days(3);
        assert_eq!(
            format_reset(Some(later), now),
            Some(format!(
                "resets {}",
                later.with_timezone(&Local).format("%a")
            ))
        );
    }

    // ---- what a plan costs (gh#178) ----

    fn login(id: &str, email: Option<&str>) -> AgentAccount {
        AgentAccount {
            id: id.into(),
            harness: HarnessId::ClaudeCode,
            email: email.map(str::to_string),
            plan_label: None,
            active: true,
            usage_windows: vec![],
            display_name: None,
            organization: None,
            auth_kind: None,
            switchable: true,
            saved_at: None,
        }
    }

    fn configured(entries: &[(&str, AccountConfig)]) -> BTreeMap<String, AccountConfig> {
        entries
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    fn plan(monthly: f64, label: Option<&str>) -> AccountConfig {
        AccountConfig {
            email: None,
            plan: label.map(str::to_string),
            monthly_usd: comet_proto::view::rates::Usd::from_dollars(monthly),
        }
    }

    /// The plan is written under the email, because that is the spelling the
    /// board records a dispatch's payer as — but it is *found* under either,
    /// so a hand-written slot-id entry is not invisible here while the stats
    /// page is already crediting it.
    #[test]
    fn a_plan_is_found_under_whichever_name_the_file_used() {
        let account = login("8f2c1d0a7b6e4539", Some("brede@tally.no"));
        assert_eq!(plan_slot(&account), "brede@tally.no");
        // No address at all: the slot id is the key, which the board accepts.
        assert_eq!(plan_slot(&login("slot-1", None)), "slot-1");

        for entries in [
            configured(&[("brede@tally.no", plan(200.0, None))]),
            configured(&[("8f2c1d0a7b6e4539", plan(200.0, None))]),
            // Written under the slot id with the address beside it.
            configured(&[(
                "another-slot",
                AccountConfig {
                    email: Some("Brede@Tally.no".into()),
                    ..plan(200.0, None)
                },
            )]),
            // And however it was capitalised.
            configured(&[("BREDE@TALLY.NO", plan(200.0, None))]),
        ] {
            assert!(plan_for(&entries, &account).is_some(), "{entries:?}");
        }
        // Somebody else's plan is not this login's.
        let others = configured(&[("ana@example.com", plan(200.0, None))]);
        assert!(plan_for(&others, &account).is_none());
        assert!(plan_for(&BTreeMap::new(), &account).is_none());
    }

    /// An unconfigured plan asks; it never renders a zero, which would read as
    /// "this subscription is free" about a bill comet cannot see.
    #[test]
    fn an_unentered_plan_asks_rather_than_saying_free() {
        assert_eq!(plan_summary(None), "Add what this plan costs");
        assert_eq!(plan_summary(Some(&plan(200.0, None))), "$200/mo");
        assert_eq!(
            plan_summary(Some(&plan(200.0, Some("Claude Max 20x")))),
            "Claude Max 20x · $200/mo"
        );
        // A label of whitespace is not a label.
        assert_eq!(plan_summary(Some(&plan(17.5, Some("  ")))), "$17.50/mo");
    }

    #[test]
    fn provider_grouping_puts_active_first() {
        let account = |id: &str, harness: HarnessId, active: bool| AgentAccount {
            id: id.into(),
            harness,
            email: None,
            plan_label: None,
            active,
            usage_windows: vec![],
            display_name: None,
            organization: None,
            auth_kind: None,
            switchable: true,
            saved_at: None,
        };
        let snapshot = AgentAccountsSnapshot {
            accounts: vec![
                account("c1", HarnessId::ClaudeCode, false),
                account("x1", HarnessId::Codex, false),
                account("c2", HarnessId::ClaudeCode, true),
            ],
            warnings: vec![],
        };
        let claude = provider_accounts(&snapshot, HarnessId::ClaudeCode);
        let ids: Vec<&str> = claude.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["c2", "c1"], "active account leads");
        assert_eq!(provider_accounts(&snapshot, HarnessId::Codex).len(), 1);
        assert!(provider_accounts(&snapshot, HarnessId::Cursor).is_empty());
    }
}
