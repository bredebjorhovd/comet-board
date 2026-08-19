//! Auto-pick rules (gh#490): deterministic evaluation of which ready, labeled
//! tasks an enabled [`Automation`] should dispatch, and the record of why every
//! considered task was dispatched, skipped, deferred or refused.
//!
//! The planner is pure — a rule, the board's tasks in board order, and a
//! snapshot of the facts in — so the matching, ordering, limit and cooldown
//! behaviour is testable without a database or an engine. The half that reads
//! `board.db` and writes the log is the `impl SyncEngine` block at the bottom,
//! exactly as review delivery is shaped (`crate::review`). *Executing* a
//! dispatch stays in the engine's board loop, because releasing work needs the
//! `Runtime` and the space watch — and because every existing dispatch policy
//! (one live attempt, routes, capacity, billing guard, credentials) must keep
//! ruling there, not be re-implemented here.
//!
//! Idempotency is inherited, not invented: evaluation writes nothing but log
//! rows, dispatch goes through the same pipeline a keypress does (the partial
//! unique index refuses a duplicate live attempt), and every limit below is
//! measured from the database — so overlapping ticks, replayed sync events and
//! restarts converge on the same answer.

use std::collections::BTreeMap;

use anyhow::Result;

use crate::config::{Automation, RoutingConfig};
use crate::db::{AutomationDecision, AutomationLogRow, now, rfc3339};
use crate::model::{BoardState, Task, UpstreamState};
use crate::sync::{SyncEngine, route_context};

/// How long a log row's streak is kept after it last moved. The log is the
/// operator's history view, not an audit archive — the attempts a rule
/// released are the durable record.
const LOG_RETENTION_DAYS: i64 = 14;

/// What a rule decided about one considered task.
///
/// The vocabulary is the issue's: **skipped** is inherent to the task right
/// now (excluded label, not ready, already running, unrouted), **deferred** is
/// eligible-but-held by a limit that time will lift (capacity, cooldown,
/// budget), **refused** is a dispatch that was asked for — or made impossible
/// by the rule's own configuration — and did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Dispatch,
    Skip(String),
    Defer(String),
    Refuse(String),
}

impl Verdict {
    /// The word the log stores.
    pub fn word(&self) -> &'static str {
        match self {
            Verdict::Dispatch => "dispatched",
            Verdict::Skip(_) => "skipped",
            Verdict::Defer(_) => "deferred",
            Verdict::Refuse(_) => "refused",
        }
    }

    /// The word the preview speaks (gh#524) — what the next evaluation
    /// *would* do, so present tense where the log is past.
    pub fn verb(&self) -> &'static str {
        match self {
            Verdict::Dispatch => "dispatch",
            Verdict::Skip(_) => "skip",
            Verdict::Defer(_) => "defer",
            Verdict::Refuse(_) => "refuse",
        }
    }

    pub fn reason(&self) -> &str {
        match self {
            Verdict::Dispatch => "matched",
            Verdict::Skip(r) | Verdict::Defer(r) | Verdict::Refuse(r) => r,
        }
    }
}

/// One considered task's outcome for one evaluation.
#[derive(Debug, Clone)]
pub struct Decision {
    pub task_id: String,
    pub identifier: String,
    pub verdict: Verdict,
}

/// The database facts one evaluation of one rule reads, snapshotted so the
/// planner stays pure. All timestamps are the store's RFC3339 UTC strings,
/// which order lexicographically — the same comparison the store itself uses.
#[derive(Debug, Clone, Default)]
pub struct EvalFacts {
    pub now: String,
    /// Live attempts this rule has running — its `max_concurrent` meter.
    pub live_for_rule: usize,
    /// Dispatches this rule made in the last 24h — its `daily_budget` meter.
    pub dispatched_last_day: usize,
    /// Live attempts per workspace — the capacity meter, the same count
    /// `check_capacity` refuses on.
    pub workspace_live: BTreeMap<String, usize>,
    /// Per considered task: when its failure/refusal cooldown ends.
    pub cooldown_until: BTreeMap<String, String>,
}

/// Does this rule consider this task at all — the source and required-label
/// match. Everything considered gets a decision; everything else is not this
/// rule's business and stays out of its history.
pub fn considers(rule: &Automation, task: &Task) -> bool {
    if let Some(source) = rule.source.as_deref()
        && !source.eq_ignore_ascii_case(task.source.as_str())
    {
        return false;
    }
    let required: Vec<&str> = rule
        .labels
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    // No required labels matches nothing, never everything: validation refuses
    // enabling such a rule, and a half-written one must not read as a net.
    !required.is_empty()
        && required.iter().all(|need| {
            task.labels
                .iter()
                .any(|have| have.trim().eq_ignore_ascii_case(need))
        })
}

/// Evaluate one rule against the board.
///
/// `tasks` must arrive in board order — the same order `board_rows` publishes
/// — because "use the board priority order" means the first eligible row wins
/// the per-evaluation slots. Deterministic: same inputs, same decisions.
pub fn plan(cfg: &RoutingConfig, rule: &Automation, tasks: &[Task], facts: &EvalFacts) -> Vec<Decision> {
    let mut out = Vec::new();
    if !rule.enabled {
        return out;
    }
    let mut planned = 0usize;
    let mut planned_by_ws: BTreeMap<String, usize> = BTreeMap::new();
    for task in tasks.iter().filter(|t| considers(rule, t)) {
        let verdict = decide(cfg, rule, task, facts, planned, &planned_by_ws);
        if verdict == Verdict::Dispatch {
            planned += 1;
            if let Some(route) = cfg.resolve(&route_context(task)) {
                *planned_by_ws.entry(route.workspace.clone()).or_default() += 1;
            }
        }
        out.push(Decision {
            task_id: task.id.clone(),
            identifier: task.identifier.clone(),
            verdict,
        });
    }
    out
}

fn decide(
    cfg: &RoutingConfig,
    rule: &Automation,
    task: &Task,
    facts: &EvalFacts,
    planned: usize,
    planned_by_ws: &BTreeMap<String, usize>,
) -> Verdict {
    if let Some(excluded) = rule.exclude_labels.iter().map(|l| l.trim()).find(|need| {
        !need.is_empty()
            && task
                .labels
                .iter()
                .any(|have| have.trim().eq_ignore_ascii_case(need))
    }) {
        return Verdict::Skip(format!("excluded label `{excluded}` present"));
    }
    if task.upstream == UpstreamState::Gone {
        return Verdict::Skip("issue no longer exists upstream".into());
    }
    if let Some(live) = task.live_attempt() {
        return Verdict::Skip(format!(
            "already has a live attempt (chat {})",
            live.pane_id.as_deref().unwrap_or("pending")
        ));
    }
    if task.state != BoardState::Ready {
        return Verdict::Skip(format!("state is {}, not ready", task.state));
    }
    let Some(route) = cfg.resolve(&route_context(task)) else {
        return Verdict::Skip("no matching route".into());
    };
    if let Some(only) = rule.route.as_deref().map(str::trim).filter(|r| !r.is_empty())
        && !only.eq_ignore_ascii_case(route.display_name())
    {
        return Verdict::Skip(format!(
            "routes to `{}`, and this rule dispatches only `{only}`",
            route.display_name()
        ));
    }
    // The explicit billing account is the rule's consent to spend; without
    // one there is nothing an unattended dispatch may bill, and falling back
    // to the box's own login silently is the failure gh#101 exists to refuse.
    if rule.account.as_deref().map(str::trim).unwrap_or("").is_empty() {
        return Verdict::Refuse("billing account missing — set `account` on the rule".into());
    }
    if let Some(until) = facts.cooldown_until.get(&task.id)
        && facts.now.as_str() < until.as_str()
    {
        return Verdict::Defer(format!("cooldown active until {until}"));
    }
    if let Some(budget) = rule.daily_budget
        && facts.dispatched_last_day + planned >= budget
    {
        return Verdict::Defer(format!("daily budget of {budget} dispatches spent"));
    }
    if facts.live_for_rule + planned >= rule.max_concurrent.max(1) {
        return Verdict::Defer(format!(
            "rule is at {} of {} live",
            facts.live_for_rule + planned,
            rule.max_concurrent.max(1)
        ));
    }
    let ws_live = facts.workspace_live.get(&route.workspace).copied().unwrap_or(0)
        + planned_by_ws.get(&route.workspace).copied().unwrap_or(0);
    let cap = cfg.max_concurrent(route);
    if ws_live >= cap {
        return Verdict::Defer(format!(
            "space `{}` is at {ws_live} of {cap} working",
            route.workspace
        ));
    }
    if planned >= rule.max_per_eval.max(1) {
        return Verdict::Defer(format!(
            "evaluation limit of {} reached",
            rule.max_per_eval.max(1)
        ));
    }
    Verdict::Dispatch
}

/// A dispatch one evaluation asks the engine to make. Carries the whole rule
/// so the executor can build overrides and provenance without re-resolving a
/// name against a config that may be mid-edit.
#[derive(Debug, Clone)]
pub struct PlannedDispatch {
    pub rule: Automation,
    pub task_id: String,
    pub identifier: String,
}

// ---- the view the settings page and the board indicator read ---------------

/// Everything `ReadBoardAutomations` answers with. Never carries credentials
/// or webhook material: the rule is its own config (an account *slot id*, not
/// a secret), and reasons are the board's own sentences.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationsView {
    pub rules: Vec<RuleStatus>,
    /// Recent history across every rule, newest first.
    #[serde(default)]
    pub recent: Vec<AutomationLogRow>,
    /// Seconds until the next periodic reconciliation, when the loop said.
    #[serde(default)]
    pub next_eval_secs: Option<u64>,
    /// What each agent-account slot has already dispatched (gh#524) — the
    /// figure shown beside an account when a rule picks who pays. The board's
    /// own attempts data, never an external usage meter.
    #[serde(default)]
    pub accounts: Vec<AccountUsage>,
}

/// One slot's dispatch total for the current calendar month (gh#524).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountUsage {
    pub slot: String,
    pub dispatches_this_month: usize,
}

/// One considered task's would-be outcome, computed by the same [`plan`] the
/// loop runs, addressed at the rule as it stands (gh#524) — enabled or not.
/// This is how a rule proves itself against the live board before it is
/// enabled, and how an enabled one says what its next evaluation would do.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewRow {
    pub identifier: String,
    /// `dispatch` | `skip` | `defer` | `refuse` — [`Verdict::verb`].
    pub decision: String,
    /// The planner's reason verbatim; for a dispatch, what it would run —
    /// "`<runtime>` as `<account>`" — because "matched" tells an operator
    /// nothing about what is about to spend.
    pub reason: String,
}

/// Preview rows one rule carries at most. The count still says how many tasks
/// the rule considers; the rows past this are the same three verdicts again.
const PREVIEW_ROWS: usize = 12;

/// One rule with its liveness derived — what the list renders.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleStatus {
    pub rule: Automation,
    /// `enabled` | `paused` | `unhealthy`.
    pub state: String,
    /// Why it is unhealthy, when it is.
    #[serde(default)]
    pub problems: Vec<String>,
    /// Live attempts it has running now.
    pub live: usize,
    /// Dispatches in the last 24 hours.
    pub dispatched_last_day: usize,
    /// The newest history row about it, for the "most recent action" line.
    #[serde(default)]
    pub last_event: Option<AutomationLogRow>,
    /// What the rule would do right now, task by task (gh#524) — computed by
    /// the loop's own planner with the rule treated as enabled, so a paused
    /// draft shows what enabling would authorize. At most [`PREVIEW_ROWS`]
    /// rows; `considered` is the true count.
    #[serde(default)]
    pub preview: Vec<PreviewRow>,
    /// How many tasks the rule considers right now (source + label match).
    #[serde(default)]
    pub considered: usize,
}

impl AutomationsView {
    pub fn enabled_count(&self) -> usize {
        self.rules.iter().filter(|r| r.state != "paused").count()
    }

    pub fn unhealthy_count(&self) -> usize {
        self.rules.iter().filter(|r| r.state == "unhealthy").count()
    }
}

// ---- the database half -----------------------------------------------------

impl SyncEngine {
    /// Evaluate every enabled rule against the board as it stands: record the
    /// skip/defer/refuse decisions, and return the dispatches the executor
    /// should make. Cheap when no rule is enabled.
    pub fn auto_pick_plan(&self) -> Result<Vec<PlannedDispatch>> {
        let rules: Vec<&Automation> = self.cfg.automations.iter().filter(|a| a.enabled).collect();
        if rules.is_empty() {
            return Ok(Vec::new());
        }
        let before = rfc3339(chrono::Utc::now() - chrono::Duration::days(LOG_RETENTION_DAYS));
        self.db.prune_automation_log(&before)?;
        let tasks = self.board_ordered_tasks()?;
        let workspace_live = self.live_by_workspace()?;
        let mut out = Vec::new();
        for rule in rules {
            let facts = self.eval_facts(rule, &tasks, workspace_live.clone())?;
            for decision in plan(&self.cfg, rule, &tasks, &facts) {
                match &decision.verdict {
                    Verdict::Dispatch => out.push(PlannedDispatch {
                        rule: rule.clone(),
                        task_id: decision.task_id,
                        identifier: decision.identifier,
                    }),
                    verdict => {
                        self.db.record_automation_decision(&AutomationDecision {
                            rule: rule.name.clone(),
                            task_id: Some(decision.task_id),
                            identifier: Some(decision.identifier),
                            decision: verdict.word().into(),
                            reason: verdict.reason().to_string(),
                            attempt_id: None,
                        })?;
                    }
                }
            }
        }
        Ok(out)
    }

    /// The board's tasks in board order (gh#125): the same section sort
    /// `board_rows` publishes, stable within a section — so "first eligible
    /// row" is the same row an operator sees at the top of Ready. Shared by
    /// the loop's evaluation and the page's preview, which is what makes the
    /// preview incapable of disagreeing with the loop (gh#524).
    fn board_ordered_tasks(&self) -> Result<Vec<Task>> {
        let mut tasks = self.db.load_tasks()?;
        tasks.sort_by_key(|t| {
            BoardState::SECTION_ORDER
                .iter()
                .position(|s| *s == t.state)
                .unwrap_or(usize::MAX)
        });
        Ok(tasks)
    }

    /// Live attempts per workspace — the capacity meter.
    fn live_by_workspace(&self) -> Result<BTreeMap<String, usize>> {
        let mut by_ws: BTreeMap<String, usize> = BTreeMap::new();
        for a in self.db.live_attempts()? {
            *by_ws.entry(a.workspace.clone()).or_default() += 1;
        }
        Ok(by_ws)
    }

    fn eval_facts(
        &self,
        rule: &Automation,
        tasks: &[Task],
        workspace_live: BTreeMap<String, usize>,
    ) -> Result<EvalFacts> {
        let day_ago = rfc3339(chrono::Utc::now() - chrono::Duration::days(1));
        let mut cooldown_until = BTreeMap::new();
        if let Some(secs) = rule.cooldown_secs() {
            for task in tasks.iter().filter(|t| considers(rule, t)) {
                let refused = self.db.automation_last_refusal(&rule.name, &task.id)?;
                let failed = self.db.automation_last_failure(&rule.name, &task.id)?;
                // RFC3339 UTC strings order lexicographically, so max() is the
                // later of the two marks.
                if let Some(at) = refused.into_iter().chain(failed).max()
                    && let Ok(at) = chrono::DateTime::parse_from_rfc3339(&at)
                {
                    let until = at.to_utc() + chrono::Duration::seconds(secs as i64);
                    cooldown_until.insert(task.id.clone(), rfc3339(until));
                }
            }
        }
        Ok(EvalFacts {
            now: now(),
            live_for_rule: self.db.live_count_for_automation(&rule.name)?,
            dispatched_last_day: self.db.automation_dispatches_since(&rule.name, &day_ago)?,
            workspace_live,
            cooldown_until,
        })
    }

    /// Record that a planned dispatch happened.
    pub fn note_automation_dispatch(&self, planned: &PlannedDispatch, chat_id: &str) {
        let attempt_id = self
            .db
            .live_attempt_for_pane(chat_id)
            .ok()
            .flatten()
            .map(|a| a.id);
        let noted = self.db.record_automation_decision(&AutomationDecision {
            rule: planned.rule.name.clone(),
            task_id: Some(planned.task_id.clone()),
            identifier: Some(planned.identifier.clone()),
            decision: "dispatched".into(),
            reason: format!("dispatched to chat {chat_id}"),
            attempt_id,
        });
        if let Err(e) = noted {
            self.log.warn(format!("recording automation dispatch: {e:#}"));
        }
    }

    /// Record that a planned dispatch was refused by the pipeline. The reason
    /// is the pipeline's own refusal sentence; the cooldown reads this row, so
    /// a refusal reconsiders on the rule's clock rather than hot-looping.
    pub fn note_automation_refusal(&self, planned: &PlannedDispatch, error: &str) {
        let noted = self.db.record_automation_decision(&AutomationDecision {
            rule: planned.rule.name.clone(),
            task_id: Some(planned.task_id.clone()),
            identifier: Some(planned.identifier.clone()),
            decision: "refused".into(),
            reason: error.to_string(),
            attempt_id: None,
        });
        if let Err(e) = noted {
            self.log.warn(format!("recording automation refusal: {e:#}"));
        }
    }

    /// The rules with their health, history and preview — what
    /// `ReadBoardAutomations` answers. `next_eval_secs` is the loop's own
    /// answer for when the next periodic reconciliation runs.
    ///
    /// The preview (gh#524) is the loop's own [`plan`] addressed at each rule
    /// as if enabled, against the same board order and the same measured
    /// facts the loop would use — so what the page shows under "would" and
    /// what the loop does next cannot disagree, because they are one function.
    pub fn automations_view(&self, next_eval_secs: Option<u64>) -> Result<AutomationsView> {
        let day_ago = rfc3339(chrono::Utc::now() - chrono::Duration::days(1));
        let tasks = self.board_ordered_tasks()?;
        let workspace_live = self.live_by_workspace()?;
        let mut rules = Vec::new();
        for rule in &self.cfg.automations {
            let live = self.db.live_count_for_automation(&rule.name)?;
            let dispatched_last_day = self.db.automation_dispatches_since(&rule.name, &day_ago)?;
            let last_event = self
                .db
                .automation_log_recent(Some(&rule.name), 1)?
                .into_iter()
                .next();
            let facts = self.eval_facts(rule, &tasks, workspace_live.clone())?;
            let as_if_enabled = Automation {
                enabled: true,
                ..rule.clone()
            };
            let decisions = plan(&self.cfg, &as_if_enabled, &tasks, &facts);
            let considered = decisions.len();
            let preview = decisions
                .into_iter()
                .take(PREVIEW_ROWS)
                .map(|d| {
                    let reason = match &d.verdict {
                        Verdict::Dispatch => would_run_as(&self.cfg, rule, &tasks, &d.task_id),
                        other => other.reason().to_string(),
                    };
                    PreviewRow {
                        identifier: d.identifier,
                        decision: d.verdict.verb().into(),
                        reason,
                    }
                })
                .collect();
            let mut problems = Vec::new();
            if rule.enabled {
                if rule.account.as_deref().map(str::trim).unwrap_or("").is_empty() {
                    problems.push(
                        "no billing account — every candidate is refused until one is set".into(),
                    );
                }
                if let Some(event) = &last_event
                    && event.decision == "refused"
                {
                    problems.push(format!("last dispatch refused: {}", event.reason));
                }
            }
            let state = if !rule.enabled {
                "paused"
            } else if problems.is_empty() {
                "enabled"
            } else {
                "unhealthy"
            };
            rules.push(RuleStatus {
                rule: rule.clone(),
                state: state.into(),
                problems,
                live,
                dispatched_last_day,
                last_event,
                preview,
                considered,
            });
        }
        let month_start = chrono::Utc::now().format("%Y-%m-01T00:00:00Z").to_string();
        let accounts = self
            .db
            .account_dispatch_totals(&month_start)?
            .into_iter()
            .map(|(slot, dispatches_this_month)| AccountUsage {
                slot,
                dispatches_this_month,
            })
            .collect();
        Ok(AutomationsView {
            rules,
            recent: self.db.automation_log_recent(None, 40)?,
            next_eval_secs,
            accounts,
        })
    }
}

/// What a would-be dispatch runs and spends — the preview's dispatch line
/// (gh#524). The runtime is the rule's override or the resolved route's; the
/// account is the rule's own, which a dispatch verdict guarantees is set.
fn would_run_as(cfg: &RoutingConfig, rule: &Automation, tasks: &[Task], task_id: &str) -> String {
    let runtime = rule.runtime.clone().or_else(|| {
        tasks
            .iter()
            .find(|t| t.id == task_id)
            .and_then(|t| cfg.resolve(&route_context(t)))
            .map(|r| r.runtime.clone())
    });
    format!(
        "{} as {}",
        runtime.as_deref().unwrap_or("the route's runtime"),
        rule.account.as_deref().unwrap_or("nobody")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RoutingConfig;
    use crate::db::{Db, NewAttempt, UpsertTask};
    use crate::model::{Outcome, Source};

    fn cfg() -> RoutingConfig {
        toml::from_str(
            r#"
            [[route]]
            workspace = "ws"
            repo = "/tmp/r"
            runtime = "claude-code"
            "#,
        )
        .unwrap()
    }

    fn rule() -> Automation {
        Automation {
            name: "approved".into(),
            enabled: true,
            owner: Some("Brede".into()),
            source: None,
            labels: vec!["auto".into()],
            exclude_labels: vec!["blocked-on-human".into()],
            route: None,
            runtime: None,
            model: None,
            account: Some("slot-1".into()),
            max_per_eval: 1,
            max_concurrent: 1,
            daily_budget: None,
            cooldown: "30m".into(),
        }
    }

    fn seed(db: &Db, id: &str, identifier: &str, labels: &[&str]) {
        db.upsert_task(&UpsertTask {
            id: id.into(),
            source: Source::Github,
            source_id: identifier.into(),
            identifier: identifier.into(),
            title: format!("task {identifier}"),
            body: None,
            url: format!("https://github.com/{id}"),
            labels: labels.iter().map(|l| l.to_string()).collect(),
            source_state: Some("open".into()),
            upstream: crate::model::UpstreamState::Unstarted,
            updated_at: crate::db::now(),
        })
        .unwrap();
    }

    fn attempt(task: &str) -> NewAttempt {
        NewAttempt {
            task_id: task.into(),
            pane_id: None,
            workspace: "ws".into(),
            runtime: "claude-code".into(),
            worktree: None,
            branch: None,
            dispatched_by: None,
            dispatched_by_pane: None,
            base_sha: None,
            account: None,
            repo_path: None,
            dispatched_by_device: None,
            dispatched_by_user: None,
            dispatched_by_verified: false,
            billed_to: None,
            stacked_on: None,
            automation: None,
            automation_owner: None,
        }
    }

    fn tasks(db: &Db) -> Vec<Task> {
        db.load_tasks().unwrap()
    }

    fn facts() -> EvalFacts {
        EvalFacts {
            now: crate::db::now(),
            ..EvalFacts::default()
        }
    }

    fn verdict_of<'a>(plan: &'a [Decision], id: &str) -> &'a Verdict {
        &plan
            .iter()
            .find(|d| d.task_id == id)
            .unwrap_or_else(|| panic!("{id} was not considered"))
            .verdict
    }

    /// The headline behavior: a ready, dispatchable task carrying the rule's
    /// label is planned for dispatch, and a task without the label is not this
    /// rule's business at all — no decision, no history row.
    #[test]
    fn a_ready_labeled_task_is_planned_and_an_unlabeled_one_is_not_considered() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "gh:o/r#1", "gh#1", &["auto"]);
        seed(&db, "gh:o/r#2", "gh#2", &["docs"]);
        let plan = plan(&cfg(), &rule(), &tasks(&db), &facts());
        assert_eq!(plan.len(), 1, "only the labeled task is considered");
        assert_eq!(plan[0].task_id, "gh:o/r#1");
        assert_eq!(plan[0].verdict, Verdict::Dispatch);
    }

    /// Labels match case-insensitively and ALL required labels must be there
    /// — a rule for `auto` + `ready` must not fire on `auto` alone.
    #[test]
    fn all_required_labels_must_match() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "gh:o/r#1", "gh#1", &["AUTO"]);
        seed(&db, "gh:o/r#2", "gh#2", &["auto", "approved"]);
        let two = Automation {
            labels: vec!["auto".into(), "approved".into()],
            ..rule()
        };
        let plan = plan(&cfg(), &two, &tasks(&db), &facts());
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].task_id, "gh:o/r#2");
    }

    /// The skip reasons the issue names: excluded label, already running,
    /// not ready, and no route — each recorded in words an operator reads.
    #[test]
    fn exclusion_running_state_and_routelessness_skip_with_reasons() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "gh:o/r#1", "gh#1", &["auto", "blocked-on-human"]);
        seed(&db, "gh:o/r#2", "gh#2", &["auto"]);
        let a = db.insert_attempt(&attempt("gh:o/r#2")).unwrap();
        db.set_attempt_pane(a, "chat-2").unwrap();
        seed(&db, "gh:o/r#3", "gh#3", &["auto"]);
        db.store_derived_state("gh:o/r#3", BoardState::Review)
            .unwrap();

        let plan = plan(&cfg(), &rule(), &tasks(&db), &facts());
        match verdict_of(&plan, "gh:o/r#1") {
            Verdict::Skip(reason) => assert!(reason.contains("excluded label"), "{reason}"),
            other => panic!("expected skip, got {other:?}"),
        }
        match verdict_of(&plan, "gh:o/r#2") {
            Verdict::Skip(reason) => {
                assert!(reason.contains("live attempt"), "{reason}");
                assert!(reason.contains("chat-2"), "{reason}");
            }
            other => panic!("expected skip, got {other:?}"),
        }
        match verdict_of(&plan, "gh:o/r#3") {
            Verdict::Skip(reason) => assert!(reason.contains("review"), "{reason}"),
            other => panic!("expected skip, got {other:?}"),
        }

        // And with no route at all, the same task is skipped, not refused:
        // fixing routes is the operator's call, not the rule's.
        let unrouted = plan_with_cfg(&RoutingConfig::default(), &db);
        match verdict_of(&unrouted, "gh:o/r#2") {
            Verdict::Skip(_) => {}
            other => panic!("expected skip, got {other:?}"),
        }
    }

    fn plan_with_cfg(cfg: &RoutingConfig, db: &Db) -> Vec<Decision> {
        plan(cfg, &rule(), &tasks(db), &facts())
    }

    /// A rule pinned to one route dispatches only tasks that resolve to it.
    #[test]
    fn a_route_pin_skips_tasks_that_resolve_elsewhere() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "gh:o/r#1", "gh#1", &["auto"]);
        let pinned = Automation {
            route: Some("other-route".into()),
            ..rule()
        };
        let plan = plan(&cfg(), &pinned, &tasks(&db), &facts());
        match verdict_of(&plan, "gh:o/r#1") {
            Verdict::Skip(reason) => assert!(reason.contains("other-route"), "{reason}"),
            other => panic!("expected skip, got {other:?}"),
        }
    }

    /// No billing account is a refusal, not a silent fall to the box's own
    /// login (gh#101): the rule's history says why nothing ran.
    #[test]
    fn a_rule_without_an_account_refuses_its_candidates() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "gh:o/r#1", "gh#1", &["auto"]);
        let unpaid = Automation {
            account: None,
            ..rule()
        };
        let plan = plan(&cfg(), &unpaid, &tasks(&db), &facts());
        match verdict_of(&plan, "gh:o/r#1") {
            Verdict::Refuse(reason) => assert!(reason.contains("billing account"), "{reason}"),
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    /// Board order decides who gets the evaluation's slots: the first ready
    /// row wins, the rest defer on the per-evaluation limit.
    #[test]
    fn board_order_wins_the_per_evaluation_slots() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "gh:o/r#1", "gh#1", &["auto"]);
        seed(&db, "gh:o/r#2", "gh#2", &["auto"]);
        let wide = Automation {
            // Room for both in every other limit; only per-eval bites.
            max_concurrent: 5,
            ..rule()
        };
        let mut cfg = cfg();
        cfg.defaults.max_concurrent_per_workspace = 5;
        let plan = plan(&cfg, &wide, &tasks(&db), &facts());
        assert_eq!(plan[0].verdict, Verdict::Dispatch);
        match &plan[1].verdict {
            Verdict::Defer(reason) => assert!(reason.contains("evaluation limit"), "{reason}"),
            other => panic!("expected defer, got {other:?}"),
        }
    }

    /// The rule's own concurrency cap counts its live attempts.
    #[test]
    fn rule_concurrency_defers_when_its_attempts_are_live() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "gh:o/r#1", "gh#1", &["auto"]);
        let plan = plan(
            &cfg(),
            &rule(),
            &tasks(&db),
            &EvalFacts {
                live_for_rule: 1,
                ..facts()
            },
        );
        match verdict_of(&plan, "gh:o/r#1") {
            Verdict::Defer(reason) => assert!(reason.contains("1 of 1 live"), "{reason}"),
            other => panic!("expected defer, got {other:?}"),
        }
    }

    /// Workspace capacity holds — including against the evaluation's own
    /// earlier picks, so one evaluation cannot overfill a space that had one
    /// slot left.
    #[test]
    fn workspace_capacity_defers_and_counts_this_evaluations_own_picks() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "gh:o/r#1", "gh#1", &["auto"]);
        seed(&db, "gh:o/r#2", "gh#2", &["auto"]);
        let wide = Automation {
            max_per_eval: 2,
            max_concurrent: 5,
            ..rule()
        };
        let mut cfg = cfg();
        cfg.defaults.max_concurrent_per_workspace = 1;
        let plan = plan(&cfg, &wide, &tasks(&db), &facts());
        assert_eq!(plan[0].verdict, Verdict::Dispatch);
        match &plan[1].verdict {
            Verdict::Defer(reason) => assert!(reason.contains("`ws` is at 1 of 1"), "{reason}"),
            other => panic!("expected defer, got {other:?}"),
        }
    }

    /// A cooldown mark defers the task; a task past its mark is considered
    /// again. RFC3339 strings compare lexicographically, which is the whole
    /// comparison.
    #[test]
    fn a_cooldown_defers_until_it_lapses() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "gh:o/r#1", "gh#1", &["auto"]);
        let mut f = facts();
        f.cooldown_until
            .insert("gh:o/r#1".into(), "2999-01-01T00:00:00Z".into());
        let held = plan(&cfg(), &rule(), &tasks(&db), &f);
        match verdict_of(&held, "gh:o/r#1") {
            Verdict::Defer(reason) => assert!(reason.contains("cooldown"), "{reason}"),
            other => panic!("expected defer, got {other:?}"),
        }
        f.cooldown_until
            .insert("gh:o/r#1".into(), "2000-01-01T00:00:00Z".into());
        let free = plan(&cfg(), &rule(), &tasks(&db), &f);
        assert_eq!(*verdict_of(&free, "gh:o/r#1"), Verdict::Dispatch);
    }

    /// The daily budget is a rolling meter, and the evaluation's own picks
    /// spend it too.
    #[test]
    fn the_daily_budget_defers_once_spent() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "gh:o/r#1", "gh#1", &["auto"]);
        let budgeted = Automation {
            daily_budget: Some(2),
            ..rule()
        };
        let plan = plan(
            &cfg(),
            &budgeted,
            &tasks(&db),
            &EvalFacts {
                dispatched_last_day: 2,
                ..facts()
            },
        );
        match verdict_of(&plan, "gh:o/r#1") {
            Verdict::Defer(reason) => assert!(reason.contains("daily budget"), "{reason}"),
            other => panic!("expected defer, got {other:?}"),
        }
    }

    /// Pausing prevents future dispatches — the plan is simply empty — and
    /// (asserted where dispatch executes) cancels nothing already running.
    #[test]
    fn a_paused_rule_plans_nothing() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "gh:o/r#1", "gh#1", &["auto"]);
        let paused = Automation {
            enabled: false,
            ..rule()
        };
        assert!(plan(&cfg(), &paused, &tasks(&db), &facts()).is_empty());
    }

    // ---- the SyncEngine half -------------------------------------------

    struct FileEngine {
        e: SyncEngine,
        dir: tempfile::TempDir,
    }

    /// An engine over a file-backed store, so a test can drop it and open a
    /// second engine on the same `board.db` — the restart the issue's
    /// reconciliation requirement is about.
    fn engine_at(dir: tempfile::TempDir, cfg: RoutingConfig) -> FileEngine {
        let db = Db::open(&dir.path().join("board.db")).unwrap();
        let e = SyncEngine {
            db,
            cfg,
            credentials: Default::default(),
            paths: crate::config::Paths {
                config_dir: dir.path().to_path_buf(),
                state_dir: dir.path().to_path_buf(),
            },
            log: std::sync::Arc::new(crate::log::Logger::new("", false)),
            github: None,
            push_capabilities: None,
            as_user: std::rc::Rc::new(crate::sources::github::FixtureAsUser::default()),
            webhook: std::sync::Arc::new(crate::notify::HttpWebhook),
        };
        FileEngine { e, dir }
    }

    fn rule_cfg() -> RoutingConfig {
        let mut c = cfg();
        c.automations = vec![rule()];
        c
    }

    /// The evaluation records why every considered task did not dispatch, and
    /// returns the ones that should.
    #[test]
    fn auto_pick_plan_returns_dispatches_and_records_the_rest() {
        let fixture = engine_at(tempfile::tempdir().unwrap(), rule_cfg());
        let e = &fixture.e;
        seed(&e.db, "gh:o/r#1", "gh#1", &["auto"]);
        seed(&e.db, "gh:o/r#2", "gh#2", &["auto", "blocked-on-human"]);

        let planned = e.auto_pick_plan().unwrap();
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].task_id, "gh:o/r#1");
        assert_eq!(planned[0].rule.name, "approved");

        let log = e.db.automation_log_recent(Some("approved"), 10).unwrap();
        assert_eq!(log.len(), 1, "the skip is recorded, the dispatch not yet");
        assert_eq!(log[0].decision, "skipped");
        assert!(log[0].reason.contains("excluded label"), "{}", log[0].reason);

        // Running the evaluation again changes nothing: the streak collapses
        // instead of appending, and the plan is the same plan.
        let again = e.auto_pick_plan().unwrap();
        assert_eq!(again.len(), 1);
        let log = e.db.automation_log_recent(Some("approved"), 10).unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].count, 2);
    }

    /// A recorded refusal cools the task down — and the cooldown holds across
    /// a restart, because it is read off the store rather than remembered.
    #[test]
    fn a_refusal_cools_down_and_the_cooldown_survives_a_restart() {
        let FileEngine { e, dir } = engine_at(tempfile::tempdir().unwrap(), rule_cfg());
        seed(&e.db, "gh:o/r#1", "gh#1", &["auto"]);

        let planned = e.auto_pick_plan().unwrap();
        assert_eq!(planned.len(), 1);
        e.note_automation_refusal(&planned[0], "space `ws` is at 1 of 1 working");

        let held = e.auto_pick_plan().unwrap();
        assert!(held.is_empty(), "a refused task must not retry hot");
        let log = e.db.automation_log_recent(Some("approved"), 10).unwrap();
        assert!(
            log.iter()
                .any(|r| r.decision == "deferred" && r.reason.contains("cooldown")),
            "{log:?}"
        );

        // The restart: a fresh engine over the same store reaches the same
        // answer, because nothing about the cooldown lived in memory.
        drop(e);
        let reopened = engine_at(dir, rule_cfg());
        assert!(reopened.e.auto_pick_plan().unwrap().is_empty());
    }

    /// One live attempt per task stays the law: a task the rule (or anybody)
    /// already has running is skipped, so duplicate events and overlapping
    /// ticks cannot double-dispatch. The DB's partial unique index backstops
    /// even a plan that raced this check.
    #[test]
    fn a_live_attempt_skips_the_task_and_counts_against_the_rule() {
        let fixture = engine_at(tempfile::tempdir().unwrap(), rule_cfg());
        let e = &fixture.e;
        seed(&e.db, "gh:o/r#1", "gh#1", &["auto"]);
        let a = e
            .db
            .insert_attempt(&NewAttempt {
                automation: Some("approved".into()),
                automation_owner: Some("Brede".into()),
                ..attempt("gh:o/r#1")
            })
            .unwrap();
        e.db.set_attempt_pane(a, "chat-1").unwrap();

        assert!(e.auto_pick_plan().unwrap().is_empty());
        assert_eq!(e.db.live_count_for_automation("approved").unwrap(), 1);
        let log = e.db.automation_log_recent(Some("approved"), 10).unwrap();
        assert!(
            log.iter()
                .any(|r| r.decision == "skipped" && r.reason.contains("live attempt")),
            "{log:?}"
        );
    }

    /// A failed autonomous attempt cools its task down for the rule — the
    /// bounded-retry half of the cooldown.
    #[test]
    fn a_failed_attempt_cools_the_task_down() {
        let fixture = engine_at(tempfile::tempdir().unwrap(), rule_cfg());
        let e = &fixture.e;
        seed(&e.db, "gh:o/r#1", "gh#1", &["auto"]);
        let a = e
            .db
            .insert_attempt(&NewAttempt {
                automation: Some("approved".into()),
                automation_owner: Some("Brede".into()),
                ..attempt("gh:o/r#1")
            })
            .unwrap();
        e.db.close_attempt(a, Outcome::Failed).unwrap();

        let held = e.auto_pick_plan().unwrap();
        assert!(held.is_empty(), "a fresh failure must cool down, not retry");
        let log = e.db.automation_log_recent(Some("approved"), 10).unwrap();
        assert!(
            log.iter()
                .any(|r| r.decision == "deferred" && r.reason.contains("cooldown")),
            "{log:?}"
        );
    }

    /// The dispatched note lands in history with the attempt beside it.
    #[test]
    fn a_dispatch_is_recorded_with_its_attempt() {
        let fixture = engine_at(tempfile::tempdir().unwrap(), rule_cfg());
        let e = &fixture.e;
        seed(&e.db, "gh:o/r#1", "gh#1", &["auto"]);
        let planned = e.auto_pick_plan().unwrap();
        let a = e
            .db
            .insert_attempt(&NewAttempt {
                automation: Some("approved".into()),
                automation_owner: Some("Brede".into()),
                ..attempt("gh:o/r#1")
            })
            .unwrap();
        e.db.set_attempt_pane(a, "chat-9").unwrap();
        e.note_automation_dispatch(&planned[0], "chat-9");

        let log = e.db.automation_log_recent(Some("approved"), 10).unwrap();
        let dispatched = log.iter().find(|r| r.decision == "dispatched").unwrap();
        assert_eq!(dispatched.attempt_id, Some(a));
        assert!(dispatched.reason.contains("chat-9"));
    }

    /// The view derives the three states the page shows, and never carries a
    /// credential — the account is a slot id the config already names.
    #[test]
    fn the_view_reports_enabled_paused_and_unhealthy() {
        let mut c = rule_cfg();
        c.automations.push(Automation {
            name: "drafted".into(),
            enabled: false,
            ..rule()
        });
        c.automations.push(Automation {
            name: "unpaid".into(),
            account: None,
            ..rule()
        });
        let fixture = engine_at(tempfile::tempdir().unwrap(), c);
        let view = fixture.e.automations_view(Some(12)).unwrap();
        let state_of = |name: &str| {
            view.rules
                .iter()
                .find(|r| r.rule.name == name)
                .unwrap()
                .state
                .clone()
        };
        assert_eq!(state_of("approved"), "enabled");
        assert_eq!(state_of("drafted"), "paused");
        assert_eq!(state_of("unpaid"), "unhealthy");
        assert_eq!(view.next_eval_secs, Some(12));
        assert_eq!(view.enabled_count(), 2);
        assert_eq!(view.unhealthy_count(), 1);
    }

    /// gh#524: every rule carries a preview computed by the loop's own
    /// planner, addressed at the rule as if enabled — so a paused draft shows
    /// what enabling would authorize, in the planner's own words, and the
    /// dispatch line says what it would run and spend.
    #[test]
    fn the_view_previews_every_rule_with_the_loops_own_planner() {
        let mut c = rule_cfg();
        c.automations.push(Automation {
            name: "drafted".into(),
            enabled: false,
            account: None,
            ..rule()
        });
        let fixture = engine_at(tempfile::tempdir().unwrap(), c);
        seed(&fixture.e.db, "gh:o/r#1", "gh#1", &["auto"]);

        let view = fixture.e.automations_view(None).unwrap();
        let of = |name: &str| view.rules.iter().find(|r| r.rule.name == name).unwrap();

        let approved = of("approved");
        assert_eq!(approved.considered, 1);
        assert_eq!(approved.preview.len(), 1);
        assert_eq!(approved.preview[0].identifier, "gh#1");
        assert_eq!(approved.preview[0].decision, "dispatch");
        assert_eq!(approved.preview[0].reason, "claude-code as slot-1");

        // The paused draft previews too — as if enabled — and its missing
        // account speaks in the planner's own refusal sentence, which is the
        // page's help text by construction.
        let drafted = of("drafted");
        assert_eq!(drafted.preview.len(), 1);
        assert_eq!(drafted.preview[0].decision, "refuse");
        assert!(
            drafted.preview[0].reason.contains("billing account missing"),
            "{}",
            drafted.preview[0].reason
        );
    }

    /// gh#524: the view carries what each agent-account slot dispatched this
    /// month, off the attempts table — the board's own record, not a usage
    /// meter — so picking who pays shows what the slot already carries.
    #[test]
    fn the_view_reports_what_each_slot_dispatched_this_month() {
        let fixture = engine_at(tempfile::tempdir().unwrap(), rule_cfg());
        let e = &fixture.e;
        seed(&e.db, "gh:o/r#1", "gh#1", &["auto"]);
        seed(&e.db, "gh:o/r#2", "gh#2", &["auto"]);
        e.db.insert_attempt(&NewAttempt {
            account: Some("slot-1".into()),
            ..attempt("gh:o/r#1")
        })
        .unwrap();
        // The box's own login is nobody's slot and stays out of the list.
        e.db.insert_attempt(&attempt("gh:o/r#2")).unwrap();

        let view = e.automations_view(None).unwrap();
        assert_eq!(view.accounts.len(), 1);
        assert_eq!(view.accounts[0].slot, "slot-1");
        assert_eq!(view.accounts[0].dispatches_this_month, 1);
    }
}
