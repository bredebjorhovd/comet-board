//! Settings → Automations (gh#490, reworked gh#524): each auto-pick rule is
//! rendered as the sentence the planner executes, and proves itself against
//! the live board before anybody enables it.
//!
//! Board-hosted like Routing (gh#75): the rules are `routing.toml`, so every
//! edit here is a `WriteBoardConfig` op through the board crate's validating
//! writer. What the config cannot carry — live counts, health, history, and
//! the preview — arrives by `ReadBoardAutomations` from the host each rule
//! lives on.
//!
//! ## The three questions a good automations page answers in place (gh#524)
//!
//! 1. **What will this rule do?** The rule renders as one sentence with each
//!    field sitting at the point where it acts, click-to-edit. A required
//!    field with nothing in it is a *blank in the sentence* — "who pays?" in
//!    the danger hue — not a quiet chip.
//! 2. **What would it do right now?** Every rule carries a preview computed
//!    by the loop's own planner (`autopick::plan`) against the current board,
//!    with the rule treated as enabled — so the panel under a paused draft is
//!    exactly what enabling would authorize, in the planner's own reason
//!    sentences, and the preview and the loop can never disagree.
//! 3. **Did my change take?** Every write renders saving → saved-from-host /
//!    refused-on-this-field. "Saved" is the host's round-tripped echo (gh#522
//!    fences replies by host), never optimism; a refusal lands on the field
//!    that caused it, in the writer's own words.
//!
//! ## No page-level board picker (gh#524, superseding gh#514's)
//!
//! The list is the union of every board the host sweep finds. Which board a
//! rule lives on is a per-rule *fact* shown on the rule, never a page mode:
//! writes are addressed at the rule's own host, so the contradiction gh#513
//! fixed — a view of one board with writes aimed at another — cannot be
//! reassembled by a picker. A host is only ever *chosen* at creation, and
//! only when it is genuinely ambiguous (more than one device hosts a board).
//!
//! ## Creating asks three things (gh#524)
//!
//! Label, owner, account — the slots that cannot default. They ride one
//! `automationAdd` write, so the rule lands whole in one echo, paused, with
//! its preview already showing what it would have done. Enabling is a
//! separate, informed consent: the ceremony names what it authorizes, and the
//! writer refuses to enable a sentence that is missing who pays.
//!
//! ## Reads stay honest (gh#513)
//!
//! A reply is applied to the board it was asked of; a failed re-read marks
//! that board's rules stale where they render instead of leaving them dressed
//! as current; a write the host refuses re-reads rather than leaving a
//! contradicted list under the refusal.

use gpui::{
    AnyElement, Context, Entity, Hsla, SharedString, Subscription, Task, Window, div, prelude::*,
    px,
};

use comet_board::autopick::{AutomationsView, RuleStatus};
use comet_board::config::Automation;
use comet_board::db::AutomationLogRow;
use comet_board::routes::RoutingView;
use comet_proto::AgentAccount;
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
    /// Whether anybody has ever released work from this board (gh#434) —
    /// kept riding the reply so the default creation target can prefer a
    /// board with evidence.
    #[serde(default)]
    dispatched: bool,
}

/// The accounts reply, as `accounts.rs` reads it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccountsReply {
    #[serde(default)]
    accounts: Vec<AgentAccount>,
}

/// One board host this page reads and writes — the union's unit (gh#524).
/// `id: None` is this device. Every read and write is fenced to its entry:
/// a reply about one board can only ever repaint that board (gh#513).
struct HostBoard {
    id: Option<String>,
    config: Option<BoardConfig>,
    automations: Option<AutomationsView>,
    /// The agent-account slots saved on this host — what the account slot
    /// offers when a rule picks who pays.
    accounts: Vec<AgentAccount>,
    /// A read for this board is in flight — "Reading…", not "missing".
    fetching: bool,
    /// The rules on screen are a previous successful read carried past a
    /// failed re-read (gh#513) — rendered under a strip saying so.
    stale: bool,
    config_task: Option<Task<()>>,
    fetch_task: Option<Task<()>>,
    accounts_task: Option<Task<()>>,
}

impl HostBoard {
    fn new(id: Option<String>) -> Self {
        Self {
            id,
            config: None,
            automations: None,
            accounts: Vec::new(),
            fetching: false,
            stale: false,
            config_task: None,
            fetch_task: None,
            accounts_task: None,
        }
    }
}

/// One rule key this page edits inline — a slot in the sentence. Everything
/// else (a rename, a reordering) is a `routing.toml` edit somebody does
/// knowingly.
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

    /// Why the field exists, phrased as consequence (gh#524) — shown where
    /// the field is edited. The account line is the planner's own refusal
    /// sentence turned around, so help text and behavior cannot drift.
    fn purpose(self) -> &'static str {
        match self {
            RuleField::Owner => {
                "The human this rule's dispatches answer to — named on every \
                 attempt it releases. Required to enable."
            }
            RuleField::Labels => {
                "A task must carry all of these to be considered — the label that \
                 means \"approved for autonomous execution\". Comma-separated. \
                 Required to enable."
            }
            RuleField::ExcludeLabels => {
                "A task carrying any of these is skipped even when the required \
                 labels match. Empty excludes nothing."
            }
            RuleField::Account => {
                "The slot this rule's dispatches bill. Without one, every \
                 candidate is refused."
            }
            RuleField::Runtime => {
                "What a dispatch runs on. Empty runs whatever runtime the task's \
                 route names."
            }
            RuleField::Model => "Model override for dispatches. Empty runs the harness default.",
            RuleField::Route => {
                "Only dispatch tasks that resolve to this route. Empty takes \
                 whatever route each task resolves to — the rule never invents one."
            }
            RuleField::MaxPerEval => {
                "Most dispatches one evaluation may make. Evaluations run every \
                 sync cycle, so this is a rate more than a ceiling. Empty: 1."
            }
            RuleField::MaxConcurrent => {
                "Most live attempts this rule may have running at once. Empty: 1."
            }
            RuleField::DailyBudget => {
                "Most dispatches per rolling 24 hours. Empty: no daily cap — the \
                 live cap still holds."
            }
            RuleField::Cooldown => {
                "How long a task waits after a failure or refusal before this rule \
                 considers it again — what keeps a standing refusal from \
                 hot-looping. Empty: 30m. `off` reconsiders next cycle."
            }
        }
    }

    fn current(self, rule: &Automation) -> Option<String> {
        let list = |labels: &[String]| (!labels.is_empty()).then(|| labels.join(", "));
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

/// What one sentence slot says for a rule, and whether it is a **blank** — a
/// required slot with nothing in it, rendered as the question the sentence
/// cannot answer ("who pays?") rather than as a quiet default (gh#524).
fn slot_text(field: RuleField, rule: &Automation) -> (String, bool) {
    let list = |labels: &[String]| -> String {
        labels
            .iter()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let set = |value: &Option<String>| -> Option<String> {
        value
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    match field {
        RuleField::Labels => {
            let text = list(&rule.labels);
            if text.is_empty() {
                ("which label?".into(), true)
            } else {
                (text, false)
            }
        }
        RuleField::ExcludeLabels => {
            let text = list(&rule.exclude_labels);
            if text.is_empty() {
                ("nothing".into(), false)
            } else {
                (text, false)
            }
        }
        RuleField::Route => (set(&rule.route).unwrap_or_else(|| "any route".into()), false),
        RuleField::Runtime => (
            set(&rule.runtime).unwrap_or_else(|| "the route's runtime".into()),
            false,
        ),
        RuleField::Model => (
            set(&rule.model).unwrap_or_else(|| "the default model".into()),
            false,
        ),
        RuleField::Account => match set(&rule.account) {
            Some(account) => (account, false),
            None => ("who pays?".into(), true),
        },
        RuleField::Owner => match set(&rule.owner) {
            Some(owner) => (owner, false),
            None => ("who owns it?".into(), true),
        },
        RuleField::MaxPerEval => (format!("{} per check", rule.max_per_eval.max(1)), false),
        RuleField::MaxConcurrent => (format!("{} live", rule.max_concurrent.max(1)), false),
        RuleField::DailyBudget => (
            rule.daily_budget
                .map(|b| format!("{b} per day"))
                .unwrap_or_else(|| "no daily cap".into()),
            false,
        ),
        RuleField::Cooldown => {
            let cooldown = rule.cooldown.trim();
            if cooldown.is_empty() {
                ("30m cooldown".into(), false)
            } else if cooldown.eq_ignore_ascii_case("off") {
                ("no cooldown".into(), false)
            } else {
                (format!("{cooldown} cooldown"), false)
            }
        }
    }
}

/// What keeps a paused rule from being enabled — the writer's own refusals
/// (`config.rs` validates owner and labels on enable; the write seam in
/// `routes.rs` refuses enabling with nobody paying, gh#524), said before the
/// click instead of after it. Empty for an enabled rule: it got past the
/// writer.
fn enable_blockers(rule: &Automation) -> Vec<&'static str> {
    let mut out = Vec::new();
    if rule.enabled {
        return out;
    }
    if rule.owner.as_deref().map(str::trim).unwrap_or("").is_empty() {
        out.push("who owns it (owner)");
    }
    if rule.labels.iter().all(|l| l.trim().is_empty()) {
        out.push("which label it fires on (labels)");
    }
    if rule.account.as_deref().map(str::trim).unwrap_or("").is_empty() {
        out.push("who pays (account)");
    }
    out
}

/// A new rule's name, from the label it fires on — creating asks three
/// things, and a name is not one of them (gh#524). Deduped against the
/// board's rules the way the writer would refuse.
fn derive_rule_name(label: &str, existing: &[String]) -> String {
    let base = {
        let trimmed = label.trim();
        if trimmed.is_empty() {
            "rule".to_string()
        } else {
            trimmed.to_string()
        }
    };
    let mut candidate = base.clone();
    let mut n = 2;
    while existing.iter().any(|e| e.eq_ignore_ascii_case(&candidate)) {
        candidate = format!("{base} {n}");
        n += 1;
    }
    candidate
}

/// Where one write is in its life: sent, echoed back by the host, or refused
/// (gh#524). "Saved" is the host's round-tripped reply and nothing earlier —
/// the page never marks its own optimism as landed.
#[derive(Debug, Clone, PartialEq)]
enum WriteState {
    Saving,
    Saved,
    Refused(SharedString),
}

/// The one in-flight (or just-landed) write, addressed like the write itself:
/// which board, which rule, which slot. `seq` fences replies — a slow answer
/// to a superseded write must not repaint the mark of a newer one.
#[derive(Debug, Clone)]
struct WriteMark {
    seq: u64,
    host: Option<String>,
    rule: Option<String>,
    field: Option<RuleField>,
    state: WriteState,
}

struct FieldEditor {
    host: Option<String>,
    rule: String,
    field: RuleField,
    input: Entity<ComposerInput>,
    _events: Subscription,
}

/// The two confirmations, one shape: what is about to happen, said in full,
/// with the one verb that does it. Each renders inside the rule it is about.
enum Confirm {
    /// Enabling is the authorization (gh#490) — the dialog names the agents,
    /// the account and the owner it authorizes.
    Enable {
        host: Option<String>,
        rule: String,
        summary: String,
    },
    Delete {
        host: Option<String>,
        rule: String,
    },
}

impl Confirm {
    fn addresses(&self, host: &Option<String>, rule: &str) -> bool {
        match self {
            Confirm::Enable {
                host: h, rule: r, ..
            }
            | Confirm::Delete { host: h, rule: r } => h == host && r == rule,
        }
    }
}

/// The creation scaffold (gh#524): exactly the three slots that cannot
/// default, plus — only when more than one device hosts a board — which board
/// the rule is created into.
struct NewRule {
    host: Option<String>,
    label: Entity<ComposerInput>,
    owner: Entity<ComposerInput>,
    account: Entity<ComposerInput>,
    _events: Vec<Subscription>,
}

pub struct AutomationsPage {
    state: Entity<AppState>,
    /// Every device that answered the config sweep with a board — the union
    /// this page renders (gh#524). No page-level current board exists.
    boards: Vec<HostBoard>,
    editor: Option<FieldEditor>,
    confirm: Option<Confirm>,
    new_rule: Option<NewRule>,
    /// The one in-flight or last-settled write.
    mark: Option<WriteMark>,
    write_seq: u64,
    error: Option<SharedString>,
    busy: bool,
    loaded: bool,
    sweep_task: Option<Task<()>>,
    write_task: Option<Task<()>>,
    _observe: Subscription,
}

impl AutomationsPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let mut page = Self {
            state,
            boards: Vec::new(),
            editor: None,
            confirm: None,
            new_rule: None,
            mark: None,
            write_seq: 0,
            error: None,
            busy: false,
            loaded: false,
            sweep_task: None,
            write_task: None,
            _observe: observe,
        };
        page.reload(cx);
        page
    }

    fn host_params(host: &Option<String>, value: serde_json::Value) -> serde_json::Value {
        let mut value = value;
        if let (Some(host), Some(object)) = (host.as_deref(), value.as_object_mut()) {
            object.insert("targetDeviceId".into(), serde_json::json!(host));
        }
        value
    }

    /// A device's display name — `None` is this one.
    fn device_label(&self, host: Option<&str>, cx: &gpui::App) -> SharedString {
        let Some(host) = host else {
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

    fn board_ix(&self, host: &Option<String>) -> Option<usize> {
        self.boards.iter().position(|b| &b.id == host)
    }

    /// Sweep every host candidate and keep **every** board that answers —
    /// the union this page is (gh#524). No settling, no default: a board
    /// found is a board rendered.
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
        self.sweep_task = Some(cx.spawn(async move |this, cx| {
            let mut last: Option<String> = None;
            let mut found = false;
            for candidate in candidates {
                let params = Self::host_params(&candidate, serde_json::json!({}));
                match engine
                    .client()
                    .call(methods::READ_BOARD_CONFIG, params)
                    .await
                {
                    Ok(value) => match serde_json::from_value::<BoardConfig>(value) {
                        Ok(config) => {
                            found = true;
                            let updated = this.update(cx, |page, cx| {
                                let mut entry = HostBoard::new(candidate.clone());
                                entry.config = Some(config);
                                page.boards.push(entry);
                                page.fetch_automations(candidate.clone(), cx);
                                page.fetch_accounts(candidate.clone(), cx);
                                cx.notify();
                            });
                            if updated.is_err() {
                                return;
                            }
                        }
                        Err(err) => last = Some(format!("Unreadable config: {err}")),
                    },
                    Err(err) => last = Some(err.to_string()),
                }
            }
            this.update(cx, |page, cx| {
                page.loaded = true;
                if !found {
                    page.error = Some(
                        match last {
                            Some(err) => format!("No device here hosts a board ({err})"),
                            None => "No device here hosts a board".to_string(),
                        }
                        .into(),
                    );
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// A read off one board failed: the error names the board, and whatever
    /// rules of *that board* are on screen stop being presented as current
    /// (gh#513) — they are the last successful read, and the strip above them
    /// says so. Every other board is untouched.
    fn read_failed(&mut self, host: &Option<String>, err: String, cx: &gpui::App) {
        let label = self.device_label(host.as_deref(), cx);
        if let Some(ix) = self.board_ix(host) {
            let board = &mut self.boards[ix];
            board.fetching = false;
            board.stale = board.automations.is_some();
        }
        self.error = Some(format!("{label}: {err}").into());
    }

    /// Re-read one board's config (after a refused write, or from the retry
    /// strip). The reply is applied to the board it was asked of and no other.
    fn fetch_config(&mut self, host: Option<String>, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(ix) = self.board_ix(&host) else {
            return;
        };
        let params = Self::host_params(&host, serde_json::json!({}));
        self.boards[ix].fetching = true;
        let task_host = host.clone();
        self.boards[ix].config_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::READ_BOARD_CONFIG, params)
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(value) => match serde_json::from_value::<BoardConfig>(value) {
                        Ok(config) => {
                            if let Some(ix) = page.board_ix(&task_host) {
                                page.boards[ix].config = Some(config);
                            }
                            page.fetch_automations(task_host.clone(), cx);
                        }
                        Err(err) => {
                            page.read_failed(&task_host, format!("Unreadable config: {err}"), cx)
                        }
                    },
                    Err(err) => page.read_failed(&task_host, format!("{err}"), cx),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Read one board's rules, health, history and preview. Success replaces
    /// that board's view; failure marks it stale where it renders (gh#513).
    fn fetch_automations(&mut self, host: Option<String>, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(ix) = self.board_ix(&host) else {
            return;
        };
        let params = Self::host_params(&host, serde_json::json!({}));
        self.boards[ix].fetching = true;
        let task_host = host.clone();
        self.boards[ix].fetch_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::READ_BOARD_AUTOMATIONS, params)
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(value) => match serde_json::from_value::<AutomationsView>(value) {
                        Ok(view) => page.apply_automations(&task_host, view),
                        Err(err) => page.read_failed(
                            &task_host,
                            format!("Unreadable automations: {err}"),
                            cx,
                        ),
                    },
                    Err(err) => page.read_failed(&task_host, format!("{err}"), cx),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// The agent-account slots one host has saved — what the account slot
    /// offers. A failure here quietly leaves the free-text input, which still
    /// works; the accounts are a convenience, not the write path.
    fn fetch_accounts(&mut self, host: Option<String>, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(ix) = self.board_ix(&host) else {
            return;
        };
        let params = Self::host_params(&host, serde_json::json!({ "forceUsage": false }));
        let task_host = host.clone();
        self.boards[ix].accounts_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_AGENT_ACCOUNTS, params)
                .await;
            this.update(cx, |page, cx| {
                if let Ok(value) = result
                    && let Ok(reply) = serde_json::from_value::<AccountsReply>(value)
                    && let Some(ix) = page.board_ix(&task_host)
                {
                    page.boards[ix].accounts = reply.accounts;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// A fresh read of one board's rules — the one thing that makes that
    /// board's list current again.
    fn apply_automations(&mut self, host: &Option<String>, view: AutomationsView) {
        if let Some(ix) = self.board_ix(host) {
            let board = &mut self.boards[ix];
            board.fetching = false;
            board.automations = Some(view);
            board.stale = false;
        }
    }

    /// What one write's reply did to the page — split from the wire so the
    /// transitions are testable. Applies only while `seq` is still the
    /// current write: a slow reply to a superseded write repaints nothing.
    /// Returns whether the config landed (the caller re-reads the rules view
    /// on success and the config on refusal).
    fn note_write_result(
        &mut self,
        seq: u64,
        host: &Option<String>,
        result: Result<BoardConfig, String>,
    ) -> bool {
        self.busy = false;
        let current = self.mark.as_ref().is_some_and(|m| m.seq == seq);
        match result {
            Ok(config) => {
                if let Some(ix) = self.board_ix(host) {
                    self.boards[ix].config = Some(config);
                }
                if current && let Some(mark) = &mut self.mark {
                    mark.state = WriteState::Saved;
                }
                true
            }
            Err(err) => {
                if current && let Some(mark) = &mut self.mark {
                    mark.state = WriteState::Refused(err.into());
                } else {
                    self.error = Some(err.into());
                }
                false
            }
        }
    }

    /// Send one write to one board. The reply replaces that board's config
    /// and re-reads its view — a write that flipped `enabled` changes both.
    /// A refusal re-reads the config too: a name-addressed write that misses
    /// is the page showing a rule the host does not have (gh#513).
    fn write(
        &mut self,
        host: Option<String>,
        rule: Option<String>,
        field: Option<RuleField>,
        op: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        self.busy = true;
        self.error = None;
        self.write_seq += 1;
        let seq = self.write_seq;
        self.mark = Some(WriteMark {
            seq,
            host: host.clone(),
            rule,
            field,
            state: WriteState::Saving,
        });
        let params = Self::host_params(&host, op);
        self.write_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::WRITE_BOARD_CONFIG, params)
                .await;
            this.update(cx, |page, cx| {
                let outcome = match result {
                    Ok(value) => match serde_json::from_value::<BoardConfig>(value) {
                        Ok(config) => Ok(config),
                        Err(err) => Err(format!("Unreadable reply: {err}")),
                    },
                    Err(err) => Err(format!("{err}")),
                };
                if page.note_write_result(seq, &host, outcome) {
                    page.fetch_automations(host.clone(), cx);
                } else {
                    page.fetch_config(host.clone(), cx);
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn rule_config(&self, host: &Option<String>, name: &str) -> Option<&Automation> {
        let ix = self.board_ix(host)?;
        self.boards[ix]
            .config
            .as_ref()?
            .routing
            .config
            .as_ref()?
            .automations
            .iter()
            .find(|a| a.name == name)
    }

    fn rule_names(&self, host: &Option<String>) -> Vec<String> {
        self.board_ix(host)
            .and_then(|ix| self.boards[ix].config.as_ref())
            .and_then(|c| c.routing.config.as_ref())
            .map(|c| c.automations.iter().map(|a| a.name.clone()).collect())
            .unwrap_or_default()
    }

    fn open_editor(
        &mut self,
        host: Option<String>,
        rule: String,
        field: RuleField,
        cx: &mut Context<Self>,
    ) {
        let current = self.rule_config(&host, &rule).and_then(|r| field.current(r));
        let input = cx.new(|cx| ComposerInput::new(field.key(), cx));
        if let Some(current) = current {
            input.update(cx, |input, cx| input.set_text(current, cx));
        }
        let events = cx.subscribe(&input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_editor(cx);
            }
        });
        self.editor = Some(FieldEditor {
            host,
            rule,
            field,
            input,
            _events: events,
        });
        self.confirm = None;
        cx.notify();
    }

    fn submit_editor(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.take() else {
            return;
        };
        let text = editor.input.read(cx).text().trim().to_string();
        // An emptied field is a removal: the key falls back to its default,
        // which is what the editor's purpose line said clearing means.
        let value = (!text.is_empty()).then_some(text);
        self.write(
            editor.host.clone(),
            Some(editor.rule.clone()),
            Some(editor.field),
            serde_json::json!({
                "op": "automation",
                "automation": editor.rule,
                "key": editor.field.key(),
                "value": value,
            }),
            cx,
        );
    }

    /// Write one field to one value directly — the account rows' click.
    fn write_field(
        &mut self,
        host: Option<String>,
        rule: String,
        field: RuleField,
        value: String,
        cx: &mut Context<Self>,
    ) {
        self.editor = None;
        self.write(
            host,
            Some(rule.clone()),
            Some(field),
            serde_json::json!({
                "op": "automation",
                "automation": rule,
                "key": field.key(),
                "value": value,
            }),
            cx,
        );
    }

    fn open_new_rule(&mut self, cx: &mut Context<Self>) {
        if self.new_rule.is_some() {
            return;
        }
        // The default creation target: the first board somebody has released
        // work from (gh#434), else the first board found. A choice is only
        // ever shown when more than one device hosts a board.
        let host = self
            .boards
            .iter()
            .find(|b| b.config.as_ref().is_some_and(|c| c.dispatched))
            .or(self.boards.first())
            .map(|b| b.id.clone());
        let Some(host) = host else {
            return;
        };
        let label = cx.new(|cx| ComposerInput::new("label, e.g. autopick", cx));
        let owner = cx.new(|cx| ComposerInput::new("owner, e.g. Brede", cx));
        let account = cx.new(|cx| ComposerInput::new("account slot", cx));
        fn submit(
            this: &mut AutomationsPage,
            _: Entity<ComposerInput>,
            event: &ComposerInputEvent,
            cx: &mut Context<AutomationsPage>,
        ) {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_new_rule(cx);
            }
        }
        let events = vec![
            cx.subscribe(&label, submit),
            cx.subscribe(&owner, submit),
            cx.subscribe(&account, submit),
        ];
        self.new_rule = Some(NewRule {
            host,
            label,
            owner,
            account,
            _events: events,
        });
        cx.notify();
    }

    fn submit_new_rule(&mut self, cx: &mut Context<Self>) {
        let Some(flow) = &self.new_rule else {
            return;
        };
        let label = flow.label.read(cx).text().trim().to_string();
        let owner = flow.owner.read(cx).text().trim().to_string();
        let account = flow.account.read(cx).text().trim().to_string();
        let host = flow.host.clone();
        if label.is_empty() {
            self.error = Some(
                "A rule needs the label that means \"approved for autonomous execution\"."
                    .into(),
            );
            cx.notify();
            return;
        }
        let name = derive_rule_name(&label, &self.rule_names(&host));
        self.new_rule = None;
        let mut op = serde_json::json!({
            "op": "automationAdd",
            "name": name,
            "labels": label,
        });
        if let Some(object) = op.as_object_mut() {
            if !owner.is_empty() {
                object.insert("owner".into(), serde_json::json!(owner));
            }
            if !account.is_empty() {
                object.insert("account".into(), serde_json::json!(account));
            }
        }
        self.write(host, Some(name), None, op, cx);
    }

    /// Enable ceremony: everything the click would authorize, in one
    /// sentence, before the write that authorizes it.
    fn open_enable_confirm(
        &mut self,
        host: Option<String>,
        status: &RuleStatus,
        cx: &mut Context<Self>,
    ) {
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
            host,
            rule: rule.name.clone(),
            summary,
        });
        self.editor = None;
        cx.notify();
    }

    fn set_enabled(
        &mut self,
        host: Option<String>,
        rule: String,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        self.confirm = None;
        self.write(
            host,
            Some(rule.clone()),
            None,
            serde_json::json!({
                "op": "automation",
                "automation": rule,
                "key": "enabled",
                "value": if enabled { "true" } else { "false" },
            }),
            cx,
        );
    }

    fn duplicate(&mut self, host: Option<String>, rule: &str, cx: &mut Context<Self>) {
        // `<name> copy`, `<name> copy 2`, … — the writer refuses duplicates,
        // so probe against the config we hold.
        let existing = self.rule_names(&host);
        let mut candidate = format!("{rule} copy");
        let mut n = 2;
        while existing.iter().any(|e| e.eq_ignore_ascii_case(&candidate)) {
            candidate = format!("{rule} copy {n}");
            n += 1;
        }
        self.write(
            host,
            Some(candidate.clone()),
            None,
            serde_json::json!({ "op": "automationAdd", "name": candidate, "from": rule }),
            cx,
        );
    }

    fn delete(&mut self, host: Option<String>, rule: String, cx: &mut Context<Self>) {
        self.confirm = None;
        self.write(
            host,
            Some(rule.clone()),
            None,
            serde_json::json!({ "op": "automationRemove", "name": rule }),
            cx,
        );
    }

    /// Recent history across every board, newest first — one list, because
    /// the page is one list (gh#524).
    fn merged_recent(&self) -> Vec<(Option<String>, AutomationLogRow)> {
        let mut rows: Vec<(Option<String>, AutomationLogRow)> = Vec::new();
        for board in &self.boards {
            if let Some(view) = &board.automations {
                for row in &view.recent {
                    rows.push((board.id.clone(), row.clone()));
                }
            }
        }
        rows.sort_by(|a, b| b.1.last_at.cmp(&a.1.last_at));
        rows.truncate(40);
        rows
    }
}

/// The hue a decision word wears, both in the preview ("dispatch") and the
/// log ("dispatched"): the status ramp, because the words are statuses.
fn decision_color(theme: &Theme, decision: &str) -> Hsla {
    match decision {
        "dispatch" | "dispatched" => theme.settled_text(),
        "defer" | "deferred" => theme.warning_text(),
        "refuse" | "refused" => theme.danger_text(),
        _ => theme.text_subtle,
    }
}

impl Render for AutomationsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let busy = self.busy;
        let multi_board = self.boards.len() > 1;
        let rule_count: usize = self
            .boards
            .iter()
            .filter_map(|b| b.automations.as_ref())
            .map(|v| v.rules.len())
            .sum();
        let any_view = self.boards.iter().any(|b| b.automations.is_some());
        let fetching = self.boards.iter().any(|b| b.fetching);
        let next_eval = self
            .boards
            .iter()
            .filter_map(|b| b.automations.as_ref())
            .filter_map(|v| v.next_eval_secs)
            .min();

        let mut column = widgets::page_column()
            .child(widgets::page_header(
                &theme,
                "Automations",
                any_view.then_some(rule_count),
            ))
            .child(widgets::page_subtitle(
                &theme,
                if any_view {
                    let next = next_eval
                        .map(|s| format!(" Next check in {}.", board::format_age(s as i64)))
                        .unwrap_or_default();
                    format!(
                        "Each rule is the sentence the planner executes — click any part \
                         of it to change it. A new rule asks three things: label, owner, \
                         account; everything else defaults.{next}"
                    )
                } else if fetching || !self.loaded {
                    "Reading…".to_string()
                } else if self.boards.is_empty() {
                    "No board found".to_string()
                } else {
                    "Each rule is the sentence the planner executes.".to_string()
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

        if !self.boards.is_empty() {
            // Every write has a visible state (gh#524): the rule's own card
            // carries the mark, and this header repeats it — so a write whose
            // rule has no card yet (a creation saving) or no card anymore (a
            // delete landing) still lands somewhere the eye can see.
            let header_mark: Option<AnyElement> = self.mark.as_ref().map(|m| {
                let (text, color) = match &m.state {
                    WriteState::Saving => ("saving…", theme.text_subtle),
                    WriteState::Saved => ("✓ saved", theme.settled_text()),
                    WriteState::Refused(_) => ("refused", theme.danger_text()),
                };
                div()
                    .flex_none()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(color)
                    .child(SharedString::from(text))
                    .into_any_element()
            });
            // A refusal about a rule with no card on screen — a refused
            // creation, mostly — would otherwise be a word with no sentence.
            let orphan_refusal: Option<SharedString> = self.mark.as_ref().and_then(|m| {
                let WriteState::Refused(reason) = &m.state else {
                    return None;
                };
                let rendered = self
                    .boards
                    .iter()
                    .filter_map(|b| b.automations.as_ref())
                    .flat_map(|v| v.rules.iter())
                    .any(|r| Some(r.rule.name.as_str()) == m.rule.as_deref());
                (!rendered).then(|| reason.clone())
            });
            // The creation affordance is a button on the section header, not
            // a card disguised as a rule in the list it creates into (gh#514).
            column = column.child(
                div()
                    .mt(px(28.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(10.0))
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_BODY))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.text)
                                    .child(SharedString::from("Rules")),
                            )
                            .children(header_mark),
                    )
                    .child(
                        widgets::ghost_action(&theme)
                            .id("automation-new-open")
                            .border_1()
                            .border_color(theme.border)
                            .hover(widgets::ghost_hover(&theme))
                            .when(busy, |el| el.opacity(0.4))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.busy {
                                    return;
                                }
                                this.open_new_rule(cx);
                            }))
                            .child(SharedString::from("New rule")),
                    ),
            );
            if let Some(reason) = orphan_refusal {
                column = column.child(
                    widgets::error_strip(&theme, reason)
                        .id("automation-orphan-refused")
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.mark = None;
                            cx.notify();
                        })),
                );
            }
            if self.new_rule.is_some() {
                column = column.child(self.render_new_rule_editor(&theme, busy, cx));
            }
        }

        // The rules, across every board found, each card saying which board
        // it lives on when that is ambiguous. Staleness is answered where the
        // rules render (gh#513).
        let mut rule_ix = 0usize;
        let mut rendered_any = false;
        for board_ix in 0..self.boards.len() {
            let host = self.boards[board_ix].id.clone();
            let host_label = self.device_label(host.as_deref(), cx);
            if self.boards[board_ix].stale && self.boards[board_ix].automations.is_some() {
                let retry_host = host.clone();
                column = column.child(
                    widgets::warning_strip(
                        &theme,
                        format!(
                            "{host_label} did not answer the last read — its rules below \
                             are an earlier read and may be out of date. Click to re-read."
                        ),
                    )
                    .mt(px(widgets::BLOCK_GAP))
                    .id(("automations-stale", board_ix))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.fetch_config(retry_host.clone(), cx);
                        cx.notify();
                    })),
                );
            }
            if self.boards[board_ix].automations.is_none()
                && self.boards[board_ix].config.is_some()
                && !self.boards[board_ix].fetching
            {
                let retry_host = host.clone();
                column = column.child(
                    widgets::warning_strip(
                        &theme,
                        format!("The rules on {host_label} could not be read. Click to retry."),
                    )
                    .mt(px(widgets::BLOCK_GAP))
                    .id(("automations-retry", board_ix))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.fetch_config(retry_host.clone(), cx);
                        cx.notify();
                    })),
                );
            }
            let Some(view) = self.boards[board_ix].automations.clone() else {
                continue;
            };
            for status in &view.rules {
                rendered_any = true;
                column = column.child(self.render_rule(
                    &theme,
                    rule_ix,
                    &host,
                    multi_board.then(|| host_label.clone()),
                    status,
                    board_ix,
                    busy,
                    cx,
                ));
                rule_ix += 1;
            }
        }
        if any_view && !rendered_any {
            column = column.child(
                widgets::section_card(&theme).child(
                    widgets::card_row(&theme, true).child(
                        div()
                            .text_size(px(Theme::TEXT_DENSE))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(
                                "No rules yet — the board dispatches nothing on its own.",
                            )),
                    ),
                ),
            );
        }

        if any_view {
            column = column
                .child(
                    div()
                        .mt(px(28.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_size(px(Theme::TEXT_BODY))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme.text)
                                .child(SharedString::from("Recent activity")),
                        )
                        .child(
                            div()
                                .text_size(px(Theme::TEXT_DENSE))
                                .text_color(theme.text_subtle)
                                .child(SharedString::from(
                                    "why every considered task went where it went",
                                )),
                        ),
                )
                .child(self.render_history(&theme, multi_board, cx));
        }

        div()
            .id("automations-page")
            .size_full()
            .overflow_y_scroll()
            .child(column)
    }
}

impl AutomationsPage {
    /// One word of the sentence — the connective tissue between the slots.
    fn word(theme: &Theme, text: impl Into<SharedString>) -> AnyElement {
        div()
            .text_color(theme.text_muted)
            .child(text.into())
            .into_any_element()
    }

    /// One rule, as the card the mock draws: header, sentence, whatever is
    /// open inside it (editor, confirm), and the preview footer where the
    /// rule proves itself against the live board.
    #[allow(clippy::too_many_arguments)]
    fn render_rule(
        &self,
        theme: &Theme,
        ix: usize,
        host: &Option<String>,
        host_fact: Option<SharedString>,
        status: &RuleStatus,
        board_ix: usize,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rule = &status.rule;
        let name = rule.name.clone();
        let blockers = enable_blockers(rule);

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

        // Did my change take? The mark, rendered where the rule is (gh#524):
        // saving is in flight, saved is the host's echo, a refusal renders as
        // a strip below in the writer's own words.
        let mark = self
            .mark
            .as_ref()
            .filter(|m| &m.host == host && m.rule.as_deref() == Some(name.as_str()));
        let mark_indicator: Option<AnyElement> = mark.map(|m| match &m.state {
            WriteState::Saving => div()
                .flex_none()
                .text_size(px(Theme::TEXT_CAPTION))
                .text_color(theme.text_subtle)
                .child(SharedString::from("saving…"))
                .into_any_element(),
            WriteState::Saved => div()
                .flex_none()
                .text_size(px(Theme::TEXT_CAPTION))
                .text_color(theme.settled_text())
                .child(SharedString::from("✓ saved"))
                .into_any_element(),
            WriteState::Refused(_) => div()
                .flex_none()
                .text_size(px(Theme::TEXT_CAPTION))
                .text_color(theme.danger_text())
                .child(SharedString::from("refused"))
                .into_any_element(),
        });
        let refusal: Option<SharedString> = mark.and_then(|m| match &m.state {
            WriteState::Refused(reason) => Some(reason.clone()),
            _ => None,
        });
        let echoed_field: Option<RuleField> = mark.and_then(|m| {
            matches!(m.state, WriteState::Saved)
                .then_some(m.field)
                .flatten()
        });

        // A sentence slot: what it is set to, one click to edit. A blank —
        // a required slot with nothing in it — wears the danger hue and a
        // dashed border: the sentence visibly cannot finish (gh#524). The
        // slot a save just echoed wears the settled ring.
        let chip = |field: RuleField| -> AnyElement {
            let (label, missing) = slot_text(field, rule);
            let name = name.clone();
            let host = host.clone();
            let base = div()
                .id(("automation-slot", ix * 16 + field as usize))
                .flex()
                .flex_row()
                .items_center()
                .h(px(22.0))
                .px(px(8.0))
                .rounded(px(Theme::RADIUS_CHIP))
                .text_size(px(Theme::TEXT_DENSE))
                .font_weight(gpui::FontWeight::MEDIUM)
                .cursor_pointer()
                .when(busy, |el| el.opacity(0.4))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.busy {
                        return;
                    }
                    this.open_editor(host.clone(), name.clone(), field, cx);
                }));
            if missing {
                let danger = theme.danger;
                base.border_1()
                    .border_dashed()
                    .border_color(danger.opacity(0.55))
                    .text_color(theme.danger_text())
                    .hover(move |s| s.bg(danger.opacity(0.09)))
                    .child(SharedString::from(label))
                    .into_any_element()
            } else {
                let hover_bg = theme.white_alpha(0.10);
                base.bg(theme.white_alpha(0.06))
                    .text_color(theme.text)
                    .when(echoed_field == Some(field), |el| {
                        el.border_1().border_color(theme.settled.opacity(0.55))
                    })
                    .hover(move |s| s.bg(hover_bg))
                    .child(SharedString::from(label))
                    .into_any_element()
            }
        };
        // A slot with its trailing punctuation glued on, so a wrap cannot
        // strand a comma at the start of a line.
        let glued = |chip: AnyElement, suffix: &'static str| -> AnyElement {
            div()
                .flex()
                .flex_row()
                .items_center()
                .child(chip)
                .child(
                    div()
                        .text_color(theme.text_muted)
                        .child(SharedString::from(suffix)),
                )
                .into_any_element()
        };

        // The sentence (gh#524): each field at the point where it acts.
        let mut sentence = div()
            .px(px(widgets::ROW_PAD_X))
            .pb(px(widgets::ROW_PAD_Y))
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_x(px(5.0))
            .gap_y(px(8.0))
            .text_size(px(Theme::TEXT_BODY))
            .child(Self::word(theme, "When a"))
            .child(
                div()
                    .text_color(theme.text)
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(SharedString::from("ready")),
            )
            .child(Self::word(theme, "task"));
        if let Some(source) = rule.source.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            sentence = sentence
                .child(Self::word(theme, "from"))
                .child(Self::word(theme, source.to_string()));
        }
        sentence = sentence
            .child(Self::word(theme, "in"))
            .child(chip(RuleField::Route))
            .child(Self::word(theme, "carries label"))
            .child(chip(RuleField::Labels))
            .child(Self::word(theme, "and not"))
            .child(glued(chip(RuleField::ExcludeLabels), ","))
            .child(Self::word(theme, "dispatch it on"))
            .child(chip(RuleField::Runtime))
            .child(Self::word(theme, "running"))
            .child(chip(RuleField::Model))
            .child(Self::word(theme, "as"))
            .child(glued(chip(RuleField::Account), ","))
            .child(Self::word(theme, "owned by"))
            .child(glued(chip(RuleField::Owner), ","))
            .child(Self::word(theme, "holding to at most"))
            .child(chip(RuleField::MaxConcurrent))
            .child(Self::word(theme, "·"))
            .child(chip(RuleField::DailyBudget))
            .child(Self::word(theme, "·"))
            .child(chip(RuleField::MaxPerEval))
            .child(Self::word(theme, "·"))
            .child(chip(RuleField::Cooldown))
            .child(Self::word(theme, "after failure."));

        // Header: the rule's name, its state, its activity — and the verbs.
        let toggle: AnyElement = if rule.enabled {
            let name = name.clone();
            let host = host.clone();
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
                    this.set_enabled(host.clone(), name.clone(), false, cx);
                }))
                .child(SharedString::from("Pause"))
                .into_any_element()
        } else if blockers.is_empty() {
            let status = status.clone();
            let host = host.clone();
            widgets::ghost_action(theme)
                .id(("automation-enable", ix))
                .border_1()
                .border_color(theme.border)
                .hover(widgets::ghost_hover(theme))
                .when(busy, |el| el.opacity(0.4))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.busy {
                        return;
                    }
                    this.open_enable_confirm(host.clone(), &status, cx);
                }))
                .child(SharedString::from("Enable…"))
                .into_any_element()
        } else {
            // Not clickable: the sentence is missing answers, and the strip
            // in the preview footer says which. A dead verb that says why is
            // better than a live one that refuses after the click.
            widgets::ghost_action(theme)
                .id(("automation-enable-blocked", ix))
                .border_1()
                .border_color(theme.border)
                .opacity(0.4)
                .child(SharedString::from("Enable"))
                .into_any_element()
        };

        let mut header = div()
            .px(px(widgets::ROW_PAD_X))
            .pt(px(12.0))
            .pb(px(8.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(widgets::row_title(theme, rule.name.clone()))
            .child(state_badge)
            .child(
                div()
                    .flex_none()
                    .text_size(px(Theme::TEXT_DENSE))
                    .text_color(theme.text_subtle)
                    .child(SharedString::from(format!(
                        "{} live · {} today",
                        status.live, status.dispatched_last_day
                    ))),
            );
        if let Some(fact) = host_fact {
            // Which board this rule lives on — a fact on the rule, never a
            // page mode (gh#524). Writes on this card go there and nowhere.
            header = header.child(
                div()
                    .flex_none()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(format!("on {fact}"))),
            );
        }
        header = header.child(div().flex_1());
        if let Some(indicator) = mark_indicator {
            header = header.child(indicator);
        }
        let dup_name = name.clone();
        let dup_host = host.clone();
        let del_name = name.clone();
        let del_host = host.clone();
        header = header
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
                        this.duplicate(dup_host.clone(), &dup_name, cx);
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
                            host: del_host.clone(),
                            rule: del_name.clone(),
                        });
                        this.editor = None;
                        cx.notify();
                    }))
                    .child(SharedString::from("Delete…")),
            );

        let mut card = widgets::section_card(theme).child(header);

        // A refused write lands on the rule it addressed, in the writer's own
        // words — nothing on this page fails silently (gh#524).
        if let Some(reason) = refusal {
            card = card.child(
                div()
                    .px(px(widgets::ROW_PAD_X))
                    .pb(px(8.0))
                    .child(widgets::error_strip(theme, reason).mt(px(0.0)).id((
                        "automation-refused",
                        ix,
                    ))),
            );
        }

        card = card.child(sentence);

        if let Some(editor) = &self.editor
            && &editor.host == host
            && editor.rule == rule.name
        {
            card = card.child(self.render_field_editor(theme, editor, board_ix, ix, cx));
        }
        if let Some(confirm) = &self.confirm
            && confirm.addresses(host, &rule.name)
        {
            card = card.child(self.render_confirm(theme, confirm, busy, cx));
        }

        card = card.child(self.render_preview(theme, ix, status, &blockers));

        card.into_any_element()
    }

    /// The preview footer (gh#524): the rule proving itself against the live
    /// board, in the planner's own words — the same `autopick::plan` the loop
    /// runs, addressed at this rule. An enabled rule reads as "next evaluation
    /// would"; a paused one as what enabling would authorize.
    fn render_preview(
        &self,
        theme: &Theme,
        ix: usize,
        status: &RuleStatus,
        blockers: &[&'static str],
    ) -> AnyElement {
        let enabled = status.rule.enabled;
        let dot = if !blockers.is_empty() || status.preview.iter().any(|r| r.decision == "refuse")
        {
            theme.danger
        } else if enabled {
            theme.warning
        } else {
            theme.text_faint
        };
        let title = if enabled {
            "Next evaluation would"
        } else {
            "If enabled, right now this would"
        };
        let counts = if status.considered == 0 {
            " · no task on the board carries this rule's labels".to_string()
        } else {
            format!(
                " · same planner the loop runs · considers {} task{}",
                status.considered,
                if status.considered == 1 { "" } else { "s" }
            )
        };

        let mut footer = div()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.white_alpha(0.015))
            .px(px(widgets::ROW_PAD_X))
            .py(px(12.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    // round-ok: the preview's status dot.
                    .child(div().flex_none().size(px(6.0)).rounded_full().bg(dot))
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text_muted)
                            .child(SharedString::from(title)),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(theme.text_subtle)
                            .child(SharedString::from(counts)),
                    ),
            );

        for (pix, row) in status.preview.iter().enumerate() {
            footer = footer.child(
                div()
                    .id(("automation-preview", ix * 32 + pix))
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(px(10.0))
                    .text_size(px(Theme::TEXT_DENSE))
                    .child(
                        div()
                            .flex_none()
                            .w(px(72.0))
                            .font_family(theme.font_mono.clone())
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(theme.text)
                            .child(SharedString::from(row.identifier.clone())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(64.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(decision_color(theme, &row.decision))
                            .child(SharedString::from(row.decision.clone())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_color(theme.text_subtle)
                            .child(SharedString::from(row.reason.clone())),
                    ),
            );
        }
        if status.considered > status.preview.len() {
            footer = footer.child(
                div()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(format!(
                        "+{} more considered",
                        status.considered - status.preview.len()
                    ))),
            );
        }

        if !blockers.is_empty() {
            footer = footer.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(7.0))
                    .text_size(px(Theme::TEXT_DENSE))
                    .text_color(theme.danger_text())
                    .child(SharedString::from(format!(
                        "Can't enable — the sentence is missing {}.",
                        blockers.join(" and ")
                    ))),
            );
        }
        for (pix, problem) in status.problems.iter().enumerate() {
            footer = footer.child(
                widgets::warning_strip(theme, problem.clone())
                    .id(("automation-problem", ix * 8 + pix)),
            );
        }

        footer.into_any_element()
    }

    /// One agent-account row: who it is, what plan, and what it has already
    /// dispatched this month — the board's own totals, never an external
    /// usage meter (gh#524).
    fn render_account_row(
        &self,
        theme: &Theme,
        id: (&'static str, usize),
        account: &AgentAccount,
        dispatched: Option<usize>,
        on_pick: impl Fn(&mut Self, String, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let title = account
            .email
            .clone()
            .or_else(|| account.display_name.clone())
            .unwrap_or_else(|| account.id.clone());
        let initial = title
            .chars()
            .find(|c| c.is_alphanumeric())
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "?".into());
        let plan = crate::settings::accounts::plan_slot(account);
        let slot = account.id.clone();
        div()
            .id(id)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(9.0))
            .px(px(9.0))
            .py(px(7.0))
            .rounded(px(Theme::RADIUS_CHIP))
            .cursor_pointer()
            .hover(|s| s.bg(theme.white_alpha(0.04)))
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.busy {
                    return;
                }
                on_pick(this, slot.clone(), cx);
            }))
            .child(
                div()
                    .flex_none()
                    .size(px(22.0))
                    // round-ok: an avatar.
                    .rounded_full()
                    .border_1()
                    .border_color(theme.border)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(initial)),
            )
            .child(
                div()
                    .text_size(px(Theme::TEXT_DENSE))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(SharedString::from(title)),
            )
            .child(widgets::badge(theme, plan))
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(theme.text_subtle)
                    .child(SharedString::from(match dispatched {
                        Some(n) => format!("{n} dispatches · this month"),
                        None => "no dispatches yet".to_string(),
                    })),
            )
            .into_any_element()
    }

    /// The one-slot editor, rendered inside the rule whose sentence it edits:
    /// the field's purpose as consequence, the input, and — for the account
    /// slot — the host's saved accounts with their totals, one click each.
    fn render_field_editor(
        &self,
        theme: &Theme,
        editor: &FieldEditor,
        board_ix: usize,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut panel = div()
            .mx(px(widgets::ROW_PAD_X))
            .mb(px(12.0))
            .w_auto()
            .p(px(12.0))
            .rounded(px(Theme::RADIUS_ROW))
            .border_1()
            .border_color(theme.border)
            .bg(theme.white_alpha(0.02))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .text_size(px(Theme::TEXT_DENSE))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(SharedString::from(editor.field.key())),
            )
            .child(
                div()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .line_height(px(16.0))
                    .text_color(theme.text_subtle)
                    .child(SharedString::from(editor.field.purpose())),
            );

        if editor.field == RuleField::Account {
            let board = &self.boards[board_ix];
            let usage: Vec<(String, usize)> = board
                .automations
                .as_ref()
                .map(|v| {
                    v.accounts
                        .iter()
                        .map(|a| (a.slot.clone(), a.dispatches_this_month))
                        .collect()
                })
                .unwrap_or_default();
            let host = editor.host.clone();
            let rule = editor.rule.clone();
            let accounts = board.accounts.clone();
            for (aix, account) in accounts.iter().enumerate() {
                let dispatched = usage
                    .iter()
                    .find(|(slot, _)| slot == &account.id)
                    .map(|(_, n)| *n);
                let host = host.clone();
                let rule = rule.clone();
                panel = panel.child(self.render_account_row(
                    theme,
                    ("automation-account", ix * 16 + aix),
                    account,
                    dispatched,
                    move |this, slot, cx| {
                        this.write_field(host.clone(), rule.clone(), RuleField::Account, slot, cx);
                    },
                    cx,
                ));
            }
        }

        panel = panel.child(div().w_full().child(editor.input.clone())).child(
            div()
                .flex()
                .flex_row()
                .gap(px(6.0))
                .child(
                    widgets::ghost_action(theme)
                        .id("automation-editor-save")
                        .hover(widgets::ghost_hover(theme))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.submit_editor(cx);
                        }))
                        .child(SharedString::from("Save")),
                )
                .child(
                    widgets::ghost_action(theme)
                        .id("automation-editor-cancel")
                        .hover(widgets::ghost_hover(theme))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.editor = None;
                            cx.notify();
                        }))
                        .child(SharedString::from("Cancel")),
                ),
        );
        panel.into_any_element()
    }

    /// A confirmation rendered inside the rule it is about, in the hue of
    /// what it does: enabling in the working hue, deleting in the blocked one.
    fn render_confirm(
        &self,
        theme: &Theme,
        confirm: &Confirm,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (copy, verb, hue, action): (
            String,
            &str,
            gpui::Hsla,
            Box<dyn Fn(&mut Self, &mut Context<Self>)>,
        ) = match confirm {
            Confirm::Enable {
                host,
                rule,
                summary,
            } => {
                let rule = rule.clone();
                let host = host.clone();
                (
                    summary.clone(),
                    "Enable",
                    theme.warning,
                    Box::new(move |this, cx| {
                        this.set_enabled(host.clone(), rule.clone(), true, cx)
                    }),
                )
            }
            Confirm::Delete { host, rule } => {
                let name = rule.clone();
                let host = host.clone();
                (
                    format!(
                        "Delete “{name}”? Future dispatches stop; work already running \
                         is not cancelled, and its history stays on this page until it \
                         ages out."
                    ),
                    "Delete",
                    theme.danger,
                    Box::new(move |this, cx| this.delete(host.clone(), name.clone(), cx)),
                )
            }
        };
        div()
            .mx(px(widgets::ROW_PAD_X))
            .mb(px(12.0))
            .p(px(12.0))
            .rounded(px(Theme::RADIUS_ROW))
            .border_1()
            .border_color(hue.opacity(0.30))
            .bg(hue.opacity(0.06))
            .flex()
            .flex_col()
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
            )
            .into_any_element()
    }

    /// The creation scaffold (gh#524): three questions — label, owner,
    /// account — everything else defaults and is edited later by clicking the
    /// sentence. The rule arrives paused, preview already answering.
    fn render_new_rule_editor(
        &self,
        theme: &Theme,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(flow) = &self.new_rule else {
            return div().into_any_element();
        };
        let field = |title: &'static str, hint: &'static str, input: Entity<ComposerInput>| {
            div()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .child(
                    div()
                        .text_size(px(Theme::TEXT_CAPTION))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.text)
                        .child(SharedString::from(title)),
                )
                .child(
                    div()
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(theme.text_subtle)
                        .child(SharedString::from(hint)),
                )
                .child(div().w_full().child(input))
        };

        let mut card = widgets::section_card(theme).child(
            widgets::card_row(theme, true)
                .flex_col()
                .items_start()
                .gap(px(10.0))
                .child(widgets::row_title(theme, "New rule"))
                .child(
                    div()
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(theme.text_subtle)
                        .child(SharedString::from(
                            "Three answers; everything else defaults (any route, 1 live, \
                             30m cooldown) and is edited later by clicking the sentence. \
                             The rule arrives paused, with its preview already showing \
                             what it would have done.",
                        )),
                )
                .child(field(
                    "label",
                    "The label that means \"approved for autonomous execution\" — \
                     a task must carry it to be considered.",
                    flow.label.clone(),
                ))
                .child(field(
                    "owner",
                    "The human this rule's dispatches answer to.",
                    flow.owner.clone(),
                ))
                .child(field(
                    "account",
                    "The slot dispatches bill — without one every candidate is refused. \
                     Pick one below or type a slot id.",
                    flow.account.clone(),
                )),
        );

        // The host's saved accounts, with the board's own totals — click one
        // to answer "who pays" (gh#524, operator decision 2).
        if let Some(board_ix) = self.board_ix(&flow.host) {
            let board = &self.boards[board_ix];
            let usage: Vec<(String, usize)> = board
                .automations
                .as_ref()
                .map(|v| {
                    v.accounts
                        .iter()
                        .map(|a| (a.slot.clone(), a.dispatches_this_month))
                        .collect()
                })
                .unwrap_or_default();
            let accounts = board.accounts.clone();
            if !accounts.is_empty() {
                let mut rows = div()
                    .px(px(widgets::ROW_PAD_X - 9.0))
                    .pb(px(8.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0));
                for (aix, account) in accounts.iter().enumerate() {
                    let dispatched = usage
                        .iter()
                        .find(|(slot, _)| slot == &account.id)
                        .map(|(_, n)| *n);
                    rows = rows.child(self.render_account_row(
                        theme,
                        ("automation-new-account", aix),
                        account,
                        dispatched,
                        move |this, slot, cx| {
                            if let Some(flow) = &this.new_rule {
                                flow.account
                                    .update(cx, |input, cx| input.set_text(slot, cx));
                            }
                        },
                        cx,
                    ));
                }
                card = card.child(rows);
            }
        }

        // Which board the rule is created into — asked only when it is
        // genuinely ambiguous: more than one device hosts a board (gh#524).
        if self.boards.len() > 1 {
            let mut row = div()
                .px(px(widgets::ROW_PAD_X))
                .pb(px(10.0))
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(theme.text_faint)
                        .child(SharedString::from("on board")),
                );
            for (bix, board) in self.boards.iter().enumerate() {
                let label = self.device_label(board.id.as_deref(), cx);
                if board.id == flow.host {
                    row = row.child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .h(px(widgets::ACTION_HEIGHT))
                            .px(px(widgets::ACTION_PAD_X))
                            .rounded(px(Theme::RADIUS_CHIP))
                            .border_1()
                            .border_color(theme.settled.opacity(0.35))
                            .bg(theme.settled.opacity(0.14))
                            .text_size(px(Theme::TEXT_DENSE))
                            .text_color(theme.settled_text())
                            .child(label),
                    );
                } else {
                    let target = board.id.clone();
                    row = row.child(
                        widgets::ghost_action(theme)
                            .id(("automation-new-host", bix))
                            .border_1()
                            .border_color(theme.border)
                            .hover(widgets::ghost_hover(theme))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(flow) = &mut this.new_rule {
                                    flow.host = target.clone();
                                }
                                cx.notify();
                            }))
                            .child(label),
                    );
                }
            }
            card = card.child(row);
        }

        card = card.child(
            div()
                .px(px(widgets::ROW_PAD_X))
                .pb(px(widgets::ROW_PAD_Y))
                .flex()
                .flex_row()
                .gap(px(8.0))
                .child(
                    widgets::ghost_action(theme)
                        .id("automation-new-create")
                        .border_1()
                        .border_color(theme.border)
                        .hover(widgets::ghost_hover(theme))
                        .when(busy, |el| el.opacity(0.4))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if this.busy {
                                return;
                            }
                            this.submit_new_rule(cx);
                        }))
                        .child(SharedString::from("Create paused")),
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
        );
        card.into_any_element()
    }

    /// The history card: one line per streak, newest first, across every
    /// board — time, task, decision, reason, and the streak count.
    fn render_history(
        &self,
        theme: &Theme,
        multi_board: bool,
        cx: &gpui::App,
    ) -> AnyElement {
        let rows = self.merged_recent();
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
        for (ix, (host, row)) in rows.iter().enumerate() {
            let age = chrono::DateTime::parse_from_rfc3339(&row.last_at)
                .map(|at| {
                    let secs =
                        (chrono::Utc::now() - at.with_timezone(&chrono::Utc)).num_seconds();
                    format!("{} ago", board::format_age(secs))
                })
                .unwrap_or_else(|_| row.last_at.clone());
            let subject = row
                .identifier
                .clone()
                .or(row.task_id.clone())
                .unwrap_or_default();
            let mut detail = format!("{} · {}", row.reason, row.rule);
            if multi_board {
                detail.push_str(&format!(" on {}", self.device_label(host.as_deref(), cx)));
            }
            card = card.child(
                widgets::card_row(theme, ix == 0)
                    .items_start()
                    .child(
                        div()
                            .flex_none()
                            .w(px(64.0))
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(theme.text_faint)
                            .child(SharedString::from(age)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(72.0))
                            .font_family(theme.font_mono.clone())
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(theme.text)
                            .child(SharedString::from(subject)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(80.0))
                            .text_size(px(Theme::TEXT_DENSE))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(decision_color(theme, &row.decision))
                            .child(SharedString::from(row.decision.clone())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(Theme::TEXT_DENSE))
                            .line_height(px(18.0))
                            .text_color(theme.text_subtle)
                            .child(SharedString::from(detail)),
                    )
                    .when(row.count > 1, |el| {
                        el.child(
                            div()
                                .flex_none()
                                .px(px(7.0))
                                .rounded(px(Theme::RADIUS_CHIP))
                                .bg(theme.white_alpha(0.06))
                                .text_size(px(Theme::TEXT_CAPTION))
                                .text_color(theme.text_subtle)
                                .child(SharedString::from(format!("×{}", row.count))),
                        )
                    }),
            );
        }
        card.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(value: serde_json::Value) -> Automation {
        serde_json::from_value(value).unwrap()
    }

    /// The blockers this page names before the enable click are exactly the
    /// writer's refusals — `config.rs` validates owner and labels on enable,
    /// and the `routes.rs` write seam refuses enabling with nobody paying
    /// (gh#524). A strip that promised more or less than the writer enforces
    /// would be wrong in one direction or the other.
    #[test]
    fn the_enable_blockers_are_the_writers_refusals() {
        let drafted = rule(serde_json::json!({ "name": "a" }));
        assert_eq!(
            enable_blockers(&drafted),
            vec![
                "who owns it (owner)",
                "which label it fires on (labels)",
                "who pays (account)"
            ]
        );

        let ready = rule(serde_json::json!({
            "name": "a",
            "owner": "brede",
            "labels": ["autopick"],
            "account": "slot-1",
        }));
        assert!(enable_blockers(&ready).is_empty());

        // Whitespace is not an owner, not a label and not an account — the
        // writer trims too.
        let hollow = rule(serde_json::json!({
            "name": "a",
            "owner": "  ",
            "labels": ["  "],
            "account": " ",
        }));
        assert_eq!(enable_blockers(&hollow).len(), 3);
    }

    /// An enabled rule reports no blockers: it got past the writer, and its
    /// live problems arrive from `RuleStatus::problems` instead.
    #[test]
    fn an_enabled_rule_is_past_blocking() {
        let enabled = rule(serde_json::json!({ "name": "a", "enabled": true }));
        assert!(enable_blockers(&enabled).is_empty());
    }

    /// gh#524: a required slot with nothing in it reads as the question the
    /// sentence cannot answer, and an optional one reads as its default —
    /// what an unfilled field *means*, said where it acts.
    #[test]
    fn empty_slots_read_as_blanks_or_defaults() {
        let drafted = rule(serde_json::json!({ "name": "a" }));
        assert_eq!(
            slot_text(RuleField::Labels, &drafted),
            ("which label?".to_string(), true)
        );
        assert_eq!(
            slot_text(RuleField::Account, &drafted),
            ("who pays?".to_string(), true)
        );
        assert_eq!(
            slot_text(RuleField::Owner, &drafted),
            ("who owns it?".to_string(), true)
        );
        assert_eq!(
            slot_text(RuleField::Route, &drafted),
            ("any route".to_string(), false)
        );
        assert_eq!(
            slot_text(RuleField::Runtime, &drafted),
            ("the route's runtime".to_string(), false)
        );
        assert_eq!(
            slot_text(RuleField::ExcludeLabels, &drafted),
            ("nothing".to_string(), false)
        );
        assert_eq!(
            slot_text(RuleField::DailyBudget, &drafted),
            ("no daily cap".to_string(), false)
        );

        let filled = rule(serde_json::json!({
            "name": "a",
            "labels": ["autopick", "bug"],
            "account": "slot-1",
            "owner": "Brede",
            "cooldown": "45m",
            "max_concurrent": 2,
            "daily_budget": 4,
        }));
        assert_eq!(
            slot_text(RuleField::Labels, &filled),
            ("autopick, bug".to_string(), false)
        );
        assert_eq!(
            slot_text(RuleField::MaxConcurrent, &filled),
            ("2 live".to_string(), false)
        );
        assert_eq!(
            slot_text(RuleField::DailyBudget, &filled),
            ("4 per day".to_string(), false)
        );
        assert_eq!(
            slot_text(RuleField::Cooldown, &filled),
            ("45m cooldown".to_string(), false)
        );

        let uncooled = rule(serde_json::json!({ "name": "a", "cooldown": "off" }));
        assert_eq!(
            slot_text(RuleField::Cooldown, &uncooled),
            ("no cooldown".to_string(), false)
        );
    }

    /// gh#524: a new rule is named by the label it fires on — creating asks
    /// three things and a name is not one of them — deduped the way the
    /// writer would refuse.
    #[test]
    fn a_new_rules_name_derives_from_its_label() {
        assert_eq!(derive_rule_name("docs", &[]), "docs");
        assert_eq!(
            derive_rule_name("docs", &["docs".into(), "Docs 2".into()]),
            "docs 3"
        );
        assert_eq!(derive_rule_name("  ", &[]), "rule");
    }

    fn board_config(dispatched: bool) -> BoardConfig {
        serde_json::from_value(serde_json::json!({
            "routing": {
                "path": "/tmp/routing.toml",
                "exists": false,
                "text": "",
                "problems": [],
                "backup": false,
            },
            "dispatched": dispatched,
        }))
        .unwrap()
    }

    fn view_of(rule_name: &str) -> AutomationsView {
        serde_json::from_value(serde_json::json!({
            "rules": [{
                "rule": { "name": rule_name },
                "state": "paused",
                "live": 0,
                "dispatchedLastDay": 0,
            }],
        }))
        .unwrap()
    }

    /// A page as constructed, minus the constructor's sweep — these tests
    /// exercise the state transitions, not the wire.
    fn page(cx: &mut gpui::TestAppContext) -> gpui::Entity<AutomationsPage> {
        let state = cx.new(|_| AppState::new());
        cx.new(|cx| {
            let observe = cx.observe(&state, |_, _, cx| cx.notify());
            AutomationsPage {
                state,
                boards: Vec::new(),
                editor: None,
                confirm: None,
                new_rule: None,
                mark: None,
                write_seq: 0,
                error: None,
                busy: false,
                loaded: false,
                sweep_task: None,
                write_task: None,
                _observe: observe,
            }
        })
    }

    fn board(id: &str, rule_name: Option<&str>) -> HostBoard {
        let mut b = HostBoard::new(Some(id.into()));
        b.config = Some(board_config(true));
        b.automations = rule_name.map(view_of);
        b
    }

    /// gh#524/gh#513: the page is a union, and every reply lands on the board
    /// it was asked of — a write's echo repaints that board's config and that
    /// board's mark, and no other board notices.
    #[gpui::test]
    async fn a_writes_echo_lands_on_the_board_it_addressed(cx: &mut gpui::TestAppContext) {
        let page = page(cx);
        page.update(cx, |page, _| {
            page.boards.push(board("box", Some("check bugs")));
            page.boards.push(board("mac", Some("docs sweep")));
            page.boards[0].config = None;
            page.write_seq = 1;
            page.mark = Some(WriteMark {
                seq: 1,
                host: Some("box".into()),
                rule: Some("check bugs".into()),
                field: Some(RuleField::Account),
                state: WriteState::Saving,
            });

            let landed =
                page.note_write_result(1, &Some("box".into()), Ok(board_config(true)));
            assert!(landed);
            assert!(page.boards[0].config.is_some(), "the echo landed there");
            assert!(
                matches!(page.mark.as_ref().unwrap().state, WriteState::Saved),
                "saved is the host's echo"
            );
            assert!(
                page.boards[1].automations.is_some(),
                "the other board never moved"
            );
        });
    }

    /// gh#524: a refusal lands on the field that caused it, in the writer's
    /// own words — and a slow reply to a superseded write repaints nothing.
    #[gpui::test]
    async fn a_refusal_marks_the_field_and_a_stale_reply_marks_nothing(
        cx: &mut gpui::TestAppContext,
    ) {
        let page = page(cx);
        page.update(cx, |page, _| {
            page.boards.push(board("box", Some("check bugs")));
            page.write_seq = 2;
            page.mark = Some(WriteMark {
                seq: 2,
                host: Some("box".into()),
                rule: Some("check bugs".into()),
                field: Some(RuleField::Account),
                state: WriteState::Saving,
            });

            // A reply for write 1 arrives after write 2 started: dropped.
            let landed = page.note_write_result(
                1,
                &Some("box".into()),
                Err("no automation named `old`".into()),
            );
            assert!(!landed);
            assert!(
                matches!(page.mark.as_ref().unwrap().state, WriteState::Saving),
                "a superseded reply must not repaint the current mark"
            );

            let landed = page.note_write_result(
                2,
                &Some("box".into()),
                Err("`x` is not a whole number".into()),
            );
            assert!(!landed);
            match &page.mark.as_ref().unwrap().state {
                WriteState::Refused(reason) => {
                    assert!(reason.contains("whole number"), "{reason}")
                }
                other => panic!("expected refusal, got {other:?}"),
            }
        });
    }

    /// gh#513: a failed read marks the board it was about stale — the last
    /// successful read stays, labeled — and only that board; a fresh read
    /// makes it current again.
    #[gpui::test]
    async fn a_failed_read_marks_only_its_board_stale(cx: &mut gpui::TestAppContext) {
        let page = page(cx);
        cx.update(|cx| {
            page.update(cx, |page, cx| {
                page.boards.push(board("box", Some("check bugs")));
                page.boards.push(board("mac", Some("docs sweep")));
                page.read_failed(&Some("box".into()), "host did not answer".into(), cx);
                assert!(page.boards[0].stale, "the list on screen is not current");
                assert!(!page.boards[1].stale, "the other board answered nothing");
                assert!(page.error.is_some());
                assert!(
                    page.boards[0].automations.is_some(),
                    "the last read stays, labeled"
                );

                page.apply_automations(&Some("box".into()), view_of("check bugs"));
                assert!(!page.boards[0].stale);
            });
        });
    }

    /// gh#513: a read that fails before anything rendered has nothing to
    /// mark — no stale strip over an empty list.
    #[gpui::test]
    async fn a_failed_read_with_nothing_on_screen_marks_nothing(
        cx: &mut gpui::TestAppContext,
    ) {
        let page = page(cx);
        cx.update(|cx| {
            page.update(cx, |page, cx| {
                page.boards.push(board("box", None));
                page.read_failed(&Some("box".into()), "host did not answer".into(), cx);
                assert!(!page.boards[0].stale);
                assert!(page.error.is_some());
            });
        });
    }

    /// gh#524: history is one list across every board, newest first — the
    /// page is a union, so its memory is too.
    #[gpui::test]
    async fn recent_activity_merges_across_boards_newest_first(
        cx: &mut gpui::TestAppContext,
    ) {
        let page = page(cx);
        page.update(cx, |page, _| {
            let mut a = board("box", Some("check bugs"));
            a.automations.as_mut().unwrap().recent = vec![serde_json::from_value(
                serde_json::json!({
                    "id": 1, "rule": "check bugs", "decision": "dispatched",
                    "reason": "dispatched to chat c1",
                    "firstAt": "2026-08-19T10:00:00Z", "lastAt": "2026-08-19T10:00:00Z",
                    "count": 1,
                }),
            )
            .unwrap()];
            let mut b = board("mac", Some("docs sweep"));
            b.automations.as_mut().unwrap().recent = vec![serde_json::from_value(
                serde_json::json!({
                    "id": 2, "rule": "docs sweep", "decision": "deferred",
                    "reason": "rule is at 1 of 1 live",
                    "firstAt": "2026-08-19T11:00:00Z", "lastAt": "2026-08-19T11:00:00Z",
                    "count": 3,
                }),
            )
            .unwrap()];
            page.boards.push(a);
            page.boards.push(b);

            let merged = page.merged_recent();
            assert_eq!(merged.len(), 2);
            assert_eq!(merged[0].1.rule, "docs sweep", "newest first");
            assert_eq!(merged[0].0.as_deref(), Some("mac"));
        });
    }
}
