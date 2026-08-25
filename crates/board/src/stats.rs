//! What the board knows about its own throughput.
//!
//! Every attempt is already recorded with when it started, when it ended, how
//! it ended, which runtime ran it, which workspace it went to, and whether an
//! agent or the operator released it. That is enough to answer the only
//! question that matters about delegating work: whether it is actually
//! finishing, and how often it has to be done twice.
//!
//! Deliberately descriptive. It reports what happened; it does not grade it.

use crate::config::{Paths, RoutingConfig};
use crate::db::Db;
use crate::log::Logger;
use crate::model::{Attempt, Outcome, Task};
use crate::prices::Prices;
use anyhow::Result;
use std::collections::BTreeMap;
use std::sync::Arc;

/// The shape lives in proto so the viewports can deserialize a `BoardStats`
/// reply without linking this crate (the [`crate::runtime::RuntimeOption`]
/// split, for the same reason). What it *contains* is this module's.
pub use comet_proto::TokenUsage;
pub use comet_proto::view::rates::human_usd;
pub use comet_proto::view::stats::{
    AgentSpend, BREAKDOWN_ROWS, BoardStats as Stats, Breakdown, BreakdownRow, DayBucket, Dimension,
    Friction, HOURS, Landing, Ranking, StatsAccountIdentity, StatsMergeBasis, TokenDay,
    human_tokens, rank_breakdown,
};

/// Whose subscription a dispatch that named no slot spent (gh#101).
///
/// Said out loud rather than left blank: work that quietly ran on the box
/// owner's plan is exactly the row the billing guard exists for, and
/// "unattributed" would hide it.
pub const THE_BOX: &str = "the box's own login";

/// What a run that never announced a model is called (gh#151).
///
/// One spelling, used by every split that is keyed on the model, so an
/// unnamed run appears as the same row in all of them.
pub const UNNAMED_MODEL: &str = "unnamed model";

/// One bucket of a [`Dimension`], on the way to a [`BreakdownRow`] (gh#227).
///
/// It carries its own model split because that is what makes the row's money
/// exact: a runtime's dollars are its tokens at the rates of the models it ran.
#[derive(Default)]
struct Cut {
    dispatches: usize,
    usage: TokenUsage,
    by_model: BTreeMap<String, TokenUsage>,
}

fn minutes(a: &Attempt) -> Option<i64> {
    let start = chrono::DateTime::parse_from_rfc3339(&a.started_at).ok()?;
    let end = chrono::DateTime::parse_from_rfc3339(a.ended_at.as_deref()?).ok()?;
    Some((end - start).num_minutes().max(0))
}

fn started_within(a: &Attempt, since_days: Option<i64>) -> bool {
    let Some(days) = since_days else {
        return true;
    };
    let Ok(start) = chrono::DateTime::parse_from_rfc3339(&a.started_at) else {
        return false;
    };
    (chrono::Utc::now() - start.with_timezone(&chrono::Utc)).num_days() < days
}

/// The attempt's start as a local instant — the day and hour buckets are read
/// by a person in a timezone, and a UTC boundary splits their evening in two.
fn started_local(a: &Attempt) -> Option<chrono::DateTime<chrono::Local>> {
    chrono::DateTime::parse_from_rfc3339(&a.started_at)
        .ok()
        .map(|t| t.with_timezone(&chrono::Local))
}

/// A percentile off a sorted slice, by nearest rank. Small-n honest: with four
/// durations the p90 IS the longest, and pretending otherwise would interpolate
/// a number no attempt ever took.
fn percentile(sorted: &[i64], p: f64) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((sorted.len() as f64) * p).ceil() as usize;
    sorted.get(rank.saturating_sub(1)).copied()
}

/// Every day the charts cover, oldest first — from the first dispatch (or the
/// start of the window) to today, quiet days included. A chart with holes in it
/// reads as missing data rather than as a quiet Sunday.
///
/// One range for every series, so the dispatch bars and the token bars under
/// them are the same days in the same order and a reader comparing them index
/// by index is comparing like with like.
fn day_range(counts: &BTreeMap<String, (usize, usize)>, since_days: Option<i64>) -> Vec<String> {
    let today = chrono::Local::now().date_naive();
    let first = match (counts.keys().next(), since_days) {
        // A window says where the chart starts even if nothing ran on day one.
        (_, Some(days)) => today - chrono::Duration::days((days - 1).max(0)),
        (Some(earliest), None) => match chrono::NaiveDate::parse_from_str(earliest, "%Y-%m-%d") {
            Ok(d) => d,
            Err(_) => return Vec::new(),
        },
        (None, None) => return Vec::new(),
    };
    let mut out = Vec::new();
    let mut day = first;
    while day <= today {
        out.push(day.format("%Y-%m-%d").to_string());
        day += chrono::Duration::days(1);
    }
    out
}

fn day_series(days: &[String], counts: &BTreeMap<String, (usize, usize)>) -> Vec<DayBucket> {
    days.iter()
        .map(|date| {
            let (dispatches, done) = counts.get(date).copied().unwrap_or((0, 0));
            DayBucket {
                date: date.clone(),
                dispatches,
                done,
            }
        })
        .collect()
}

fn token_day_series(days: &[String], counts: &BTreeMap<String, TokenUsage>) -> Vec<TokenDay> {
    days.iter()
        .map(|date| TokenDay {
            date: date.clone(),
            usage: counts.get(date).copied().unwrap_or_default(),
        })
        .collect()
}

/// The model slices to use for pricing one attempt (gh#426).
///
/// A new journal carries an exact result-level split. Older attempts fall back
/// to the one model they announced, preserving their previous estimate. A
/// malformed split that does not add back to the authoritative total also
/// falls back rather than silently losing or inventing tokens.
fn attempt_models(a: &Attempt, fallback: &str) -> Vec<comet_proto::ModelTokenUsage> {
    let Some(total) = a.tokens else {
        return Vec::new();
    };
    if let Some(rows) = &a.token_models
        && !rows.is_empty()
        && rows.iter().map(|row| row.usage).sum::<TokenUsage>() == total
    {
        // One model can appear in more than one completed result frame. Fold
        // those frames here so a mixed-model attempt contributes one dispatch
        // to each model it used, while retaining the exact token split.
        let mut by_model: BTreeMap<String, TokenUsage> = BTreeMap::new();
        for row in rows {
            by_model
                .entry(row.model.clone())
                .or_default()
                .add(row.usage);
        }
        return by_model
            .into_iter()
            .map(|(model, usage)| comet_proto::ModelTokenUsage { model, usage })
            .collect();
    }
    vec![comet_proto::ModelTokenUsage {
        model: fallback.to_string(),
        usage: total,
    }]
}

/// A complete agent split for one authoritative attempt total (gh#426).
///
/// Persistence deliberately retains journal evidence even while a resumed
/// turn is unfinished. Stats are stricter: a whole-attempt coverage count is
/// earned only when every attributed bucket adds back to the attempt total.
fn attempt_agents(a: &Attempt) -> Option<&[comet_proto::AgentTokenUsage]> {
    let total = a.tokens?;
    let rows = a.token_agents.as_deref().filter(|rows| !rows.is_empty())?;
    (rows.iter().map(|row| row.usage).sum::<TokenUsage>() == total).then_some(rows)
}

/// The window's numbers, unpriced.
///
/// Kept for callers that have no rates to hand and want the throughput half —
/// the spend is `None`, which every surface reads as "rates not configured"
/// rather than as free work. Anything that *has* a config calls
/// [`gather_priced`] instead.
pub fn gather(tasks: &[Task], since_days: Option<i64>) -> Stats {
    gather_with(tasks, since_days, None).0
}

/// The same, priced at `prices` (gh#182).
pub fn gather_priced(tasks: &[Task], since_days: Option<i64>, prices: &Prices) -> Stats {
    gather_with(tasks, since_days, Some(prices)).0
}

/// The same read with the two lossless inputs an all-board collector needs.
///
/// Not a second derivation: both values are produced by the one pass below.
/// The basis is sent only by the snapshot RPC and never persisted or streamed.
pub fn gather_mergeable_priced(
    tasks: &[Task],
    since_days: Option<i64>,
    prices: &Prices,
) -> (Stats, StatsMergeBasis) {
    gather_with(tasks, since_days, Some(prices))
}

fn gather_with(
    tasks: &[Task],
    since_days: Option<i64>,
    prices: Option<&Prices>,
) -> (Stats, StatsMergeBasis) {
    let attempts: Vec<(&Task, &Attempt)> = tasks
        .iter()
        .flat_map(|t| {
            t.attempts
                .iter()
                .filter(|attempt| attempt.board_managed)
                .map(move |attempt| (t, attempt))
        })
        .filter(|(_, a)| started_within(a, since_days))
        .collect();

    let mut outcomes: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_workspace: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_runtime: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_source: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_account: BTreeMap<String, usize> = BTreeMap::new();
    let mut durations: Vec<i64> = Vec::new();
    let mut days: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut token_days: BTreeMap<String, TokenUsage> = BTreeMap::new();
    let mut hour_of_day = vec![0usize; HOURS];
    // When crossed with where (gh#179). Not a second sweep of the rows and not
    // a new thing recorded — the same attempt already carries both its start
    // and its workspace, and keeping them apart is what made the page hide the
    // one fact worth having.
    let mut hours_by_workspace: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut live = 0;
    let mut agent_dispatched = 0;
    let mut friction = Friction::default();
    // Tokens are summed only over attempts that reported any. An attempt with
    // no record contributes nothing and is counted out of the coverage below,
    // rather than adding a zero that would quietly deflate every total.
    let mut tokens = TokenUsage::default();
    let mut attempts_with_tokens = 0usize;
    let mut attempts_with_agent_usage = 0usize;
    let mut agent_totals: BTreeMap<(comet_proto::AgentKind, Option<String>, String), TokenUsage> =
        BTreeMap::new();
    let mut tokens_by_model: BTreeMap<String, TokenUsage> = BTreeMap::new();
    let mut tokens_by_runtime: BTreeMap<String, TokenUsage> = BTreeMap::new();
    let mut tokens_by_account: BTreeMap<String, TokenUsage> = BTreeMap::new();
    // Tokens per (payer, model), which is what makes the per-account price
    // exact (gh#182): an account's spend is priced with the rates of the models
    // it actually ran, never at a board-wide average. Not on `Stats` itself —
    // it is arithmetic on the way to a figure, not a fact a page renders.
    let mut tokens_by_account_model: BTreeMap<(String, String), TokenUsage> = BTreeMap::new();
    // Context fullness (gh#271), summed on the same terms as the tokens and
    // for a different question: how close this window's attempts ran to the
    // limit of what their agents could hold. Never averaged — a mean of a
    // level says nothing, and the two figures that matter are how many got
    // close and how close the closest got.
    let mut context = comet_proto::view::stats::ContextPressure::default();
    // The window cut five ways (gh#227), keyed on the axis and the row. One
    // pass over the attempts fills all five, because every one of them is
    // already on the row: which model, which harness, which space, which
    // tracker, whose subscription.
    let mut cuts: BTreeMap<(Dimension, String), Cut> = BTreeMap::new();

    for (task, a) in &attempts {
        match a.outcome {
            Some(o) => *outcomes.entry(o.as_str().to_string()).or_default() += 1,
            None => live += 1,
        }
        let source = task.source.as_str().to_string();
        *by_workspace.entry(a.workspace.clone()).or_default() += 1;
        *by_runtime.entry(a.runtime.clone()).or_default() += 1;
        *by_source.entry(source.clone()).or_default() += 1;
        // Whose subscription it spent (gh#101). A dispatch that named no slot
        // ran on the box's own login, and saying so is the point of the row —
        // "unattributed" would hide exactly the case the guard exists for.
        let payer = a.billed_to.clone().unwrap_or_else(|| THE_BOX.to_string());
        *by_account.entry(payer.clone()).or_default() += 1;
        // What it ran, as the run itself reported it. Read here rather than
        // inside the token block below because a dispatch belongs under its
        // model whether or not the harness got round to metering it.
        let model = a.model.clone().unwrap_or_else(|| UNNAMED_MODEL.to_string());
        let model_rows = attempt_models(a, &model);
        for (dimension, label) in [
            (Dimension::Runtime, &a.runtime),
            (Dimension::Space, &a.workspace),
            (Dimension::Tracker, &source),
            (Dimension::Account, &payer),
        ] {
            let cut = cuts.entry((dimension, label.clone())).or_default();
            cut.dispatches += 1;
            if let Some(usage) = a.tokens {
                cut.usage.add(usage);
                for row in &model_rows {
                    cut.by_model
                        .entry(row.model.clone())
                        .or_default()
                        .add(row.usage);
                }
            }
        }
        // Model rows describe attempts that actually used each model, so a
        // mixed-model attempt contributes once to each row. Their dispatch
        // counts intentionally overlap; their usage and cost remain an exact,
        // non-overlapping split of the authoritative attempt total.
        if model_rows.is_empty() {
            cuts.entry((Dimension::Model, model.clone()))
                .or_default()
                .dispatches += 1;
        } else {
            for row in &model_rows {
                let cut = cuts
                    .entry((Dimension::Model, row.model.clone()))
                    .or_default();
                cut.dispatches += 1;
                cut.usage.add(row.usage);
                cut.by_model
                    .entry(row.model.clone())
                    .or_default()
                    .add(row.usage);
            }
        }
        // An agent, by either name it can be known under. Counting only
        // `dispatched_by` counted only agents the board itself dispatched,
        // which is the one kind that almost never does the dispatching — so
        // this was structurally always 0 (AGE-24).
        if a.dispatcher().is_agent() {
            agent_dispatched += 1;
        }
        friction.early_settles += a.reopened as usize;
        friction.blocked_entries += a.blocked_count as usize;
        if a.overrun_warned_at.is_some() {
            friction.overruns += 1;
        }
        if let Some(m) = minutes(a) {
            durations.push(m);
        }
        // What it spent, where it is known (gh#151). Bucketed by the model the
        // harness announced and by the runtime that ran it; an attempt whose
        // journal never named a model is still counted in the totals and in
        // the runtime split, and says so in its own row rather than being
        // dropped from a table that would then not add up.
        if let Some(usage) = a.tokens {
            tokens.add(usage);
            attempts_with_tokens += 1;
            for row in &model_rows {
                tokens_by_model
                    .entry(row.model.clone())
                    .or_default()
                    .add(row.usage);
            }
            tokens_by_runtime
                .entry(a.runtime.clone())
                .or_default()
                .add(usage);
            // And the same split by payer (gh#182) — the counts above say who
            // ran how many attempts, these say what those attempts spent.
            tokens_by_account
                .entry(payer.clone())
                .or_default()
                .add(usage);
            for row in &model_rows {
                tokens_by_account_model
                    .entry((payer.clone(), row.model.clone()))
                    .or_default()
                    .add(row.usage);
            }
            if let Some(rows) = attempt_agents(a) {
                attempts_with_agent_usage += 1;
                for row in rows {
                    agent_totals
                        .entry((row.agent, row.name.clone(), row.model.clone()))
                        .or_default()
                        .add(row.usage);
                }
            }
        }
        // …and how full its window was when anybody last looked (gh#271). The
        // reading is the *last* one the attempt reported, so for a finished
        // attempt this is where it ended up — which is the number that says
        // whether the work was shaped to fit one agent's context.
        if let Some(ctx) = a.context
            && let Some(percent) = ctx.percent()
        {
            context.attempts_reported += 1;
            if ctx.is_near_compaction(comet_proto::view::board::CONTEXT_NEAR_COMPACTION) {
                context.near_compaction += 1;
            }
            context.peak_percent = Some(context.peak_percent.unwrap_or(0).max(percent));
        }
        if let Some(start) = started_local(a) {
            use chrono::Timelike as _;
            let hour = start.hour() as usize;
            hour_of_day[hour] += 1;
            hours_by_workspace
                .entry(a.workspace.clone())
                .or_insert_with(|| vec![0usize; HOURS])[hour] += 1;
            let key = start.date_naive().format("%Y-%m-%d").to_string();
            let entry = days.entry(key.clone()).or_default();
            entry.0 += 1;
            if a.outcome == Some(Outcome::Done) {
                entry.1 += 1;
            }
            // Against the day the attempt *started*, like the dispatch bar
            // above it — an overnight run's tokens belong to the evening
            // somebody released it, which is the day they are looking for.
            if let Some(usage) = a.tokens {
                token_days.entry(key).or_default().add(usage);
            }
        }
    }
    durations.sort_unstable();

    let ended = attempts.iter().filter(|(_, a)| a.outcome.is_some()).count();
    let done = outcomes.get(Outcome::Done.as_str()).copied().unwrap_or(0);

    let touched: std::collections::HashSet<&str> =
        attempts.iter().map(|(t, _)| t.id.as_str()).collect();
    friction.retried_tasks = tasks
        .iter()
        .filter(|t| {
            t.attempts
                .iter()
                .filter(|a| started_within(a, since_days))
                .count()
                > 1
        })
        .count();

    // Where the work landed, counted per TASK and not per attempt: three goes
    // at one issue produce one pull request, and counting the attempts would
    // report the same merge three times. Only tasks touched in the window.
    //
    // The last two branches are the ones that need the ATTEMPTS and not just
    // the pull request (gh#228). A task with no PR is either an agent that
    // came back empty or an agent still typing, and those are opposite facts:
    // the first is the board wasting its time, the second is the board
    // working. Keyed on PR presence alone they are one number, and the loss
    // hides inside it.
    let mut landing = Landing::default();
    for task in tasks.iter().filter(|t| touched.contains(t.id.as_str())) {
        // A PR that exists says where the work went whatever the attempts are
        // doing — a retry running under an already-merged branch is still a
        // merge. Without one, the attempts in this window are the only witness.
        let still_going = task
            .attempts
            .iter()
            .filter(|a| started_within(a, since_days))
            .any(|a| a.outcome.is_none());
        if task.pr_merged {
            landing.merged += 1;
        } else if task.pr_open {
            landing.open += 1;
        } else if task.pr_number.is_some() {
            landing.closed_unmerged += 1;
        } else if still_going {
            landing.in_flight += 1;
        } else {
            landing.no_pr += 1;
        }
    }

    let calendar = day_range(&days, since_days);

    // What it cost (gh#182). `None` when the caller had no rates to price with,
    // which every surface says out loud rather than drawing as $0.00.
    let spend = prices.and_then(|p| {
        p.spend(
            &tokens_by_model,
            &by_account,
            &tokens_by_account_model,
            since_days,
        )
    });

    let mut agent_usage: Vec<AgentSpend> = agent_totals
        .into_iter()
        .map(|((agent, name, model), usage)| {
            let by_model = BTreeMap::from([(model.clone(), usage)]);
            let priced = prices.and_then(|p| p.price(&by_model));
            AgentSpend {
                agent,
                name,
                model,
                usage,
                list_price_api_estimate: priced.map(|(cost, _)| cost),
                unpriced_tokens: priced.map_or(0, |(_, unpriced)| unpriced),
            }
        })
        .collect();
    agent_usage.sort_by(|a, b| {
        b.list_price_api_estimate
            .cmp(&a.list_price_api_estimate)
            .then_with(|| b.usage.total().cmp(&a.usage.total()))
            .then_with(|| a.label().cmp(&b.label()))
    });

    // The five cuts, priced and ranked (gh#227). A dimension this window has
    // nothing under is left out of the vector entirely rather than sent as an
    // empty one: the toggle is built from what is here, and a segment that
    // opens onto no rows is a segment that should not have been offered.
    let full_breakdown: Vec<Breakdown> = Dimension::ALL
        .iter()
        .filter_map(|dimension| {
            let rows: Vec<BreakdownRow> = cuts
                .iter()
                .filter(|((d, _), _)| d == dimension)
                .map(|((_, label), cut)| {
                    let priced = prices.and_then(|p| p.price(&cut.by_model));
                    BreakdownRow {
                        label: label.clone(),
                        dispatches: cut.dispatches,
                        usage: cut.usage,
                        cost: priced.map(|(cost, _)| cost),
                        unpriced_tokens: priced.map_or(0, |(_, unpriced)| unpriced),
                    }
                })
                .collect();
            (!rows.is_empty()).then(|| rank_breakdown(*dimension, rows, 0))
        })
        .collect();
    let breakdown = full_breakdown
        .iter()
        .map(|cut| rank_breakdown(cut.dimension, cut.rows.clone(), BREAKDOWN_ROWS))
        .collect();

    let merge_basis = StatsMergeBasis {
        duration_minutes: durations.clone(),
        breakdown: full_breakdown,
        account_identities: by_account
            .keys()
            .map(|label| {
                let identity = if label == THE_BOX {
                    StatsAccountIdentity::BoardLocal
                } else {
                    StatsAccountIdentity::Shared {
                        account_id: label.clone(),
                    }
                };
                (label.clone(), identity)
            })
            .collect(),
    };

    let stats = Stats {
        since_days,
        attempts: attempts.len(),
        tasks_touched: touched.len(),
        outcomes,
        live,
        completion_rate: (ended > 0).then(|| done as f64 / ended as f64),
        median_minutes: percentile(&durations, 0.5),
        p90_minutes: percentile(&durations, 0.9),
        longest_minutes: durations.last().copied(),
        total_minutes: durations.iter().sum(),
        tokens,
        attempts_with_tokens,
        // The coverage line, and the same `None`-not-zero rule the completion
        // rate follows: with nothing dispatched there is no share to take. A
        // window where attempts ran and none of them reported is a genuine 0%,
        // and worth saying — it is the difference between a board that spent
        // nothing and a board that is not counting.
        token_coverage: (!attempts.is_empty())
            .then(|| attempts_with_tokens as f64 / attempts.len() as f64),
        attempts_with_agent_usage,
        agent_usage,
        landing,
        friction,
        daily: day_series(&calendar, &days),
        daily_tokens: token_day_series(&calendar, &token_days),
        hour_of_day,
        hours_by_workspace,
        by_workspace,
        by_runtime,
        by_source,
        by_account,
        breakdown,
        agent_dispatched,
        spend,
        pricing_basis: comet_proto::view::stats::PricingBasis::ListPriceApiEstimate,
        tokens_by_model,
        tokens_by_runtime,
        tokens_by_account,
        context,
        // Over every task and every attempt, never the window (gh#434): this
        // is the pane's furniture question ("has anybody ever released work
        // from this board?"), asked the way `rows.rs` counts — a row's
        // `attempts` is `task.attempts.len()`, unfiltered — so the stats
        // sweep and the board pane settle on the same host.
        dispatched: tasks.iter().any(|t| !t.attempts.is_empty()),
    };
    (stats, merge_basis)
}

pub fn run(paths: &Paths, _log: Arc<Logger>, since_days: Option<i64>) -> Result<Stats> {
    let db = Db::open(&paths.db())?;
    // Priced with whatever this box's config says (gh#182). A `routing.toml`
    // that will not parse is not a reason to refuse the throughput numbers —
    // it costs the spend half, which then reports itself as unconfigured, and
    // `doctor` is where a broken config gets named.
    let prices = RoutingConfig::load_unvalidated(&paths.routing())
        .map(|cfg| Prices::from_config(&cfg))
        .unwrap_or_else(|_| Prices::builtin());
    Ok(gather_priced(&db.load_tasks()?, since_days, &prices))
}

/// CLI rows for the attribution section, independent of whether any model had
/// a matching rate. Unknown models are evidence too, and say `unpriced`
/// instead of disappearing behind the aggregate spend branch (gh#426).
fn agent_usage_lines(s: &Stats, max_rows: usize) -> Vec<String> {
    if s.agent_usage.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![format!(
        "    by agent/model list-price API estimate (detail from {}/{} attempts that reported usage):",
        s.attempts_with_agent_usage, s.attempts_with_tokens
    )];
    lines.extend(s.agent_usage.iter().take(max_rows).map(|row| {
        format!(
            "      {} · {} · {}",
            row.price_label()
                .unwrap_or_else(|| "rates not configured".to_string()),
            human_tokens(row.usage.total()),
            row.label()
        )
    }));
    let remaining = s.agent_usage.len().saturating_sub(max_rows);
    if remaining > 0 {
        lines.push(format!("      … {remaining} more agent/model row(s)"));
    }
    lines
}

pub fn print(s: &Stats) {
    if s.attempts == 0 {
        println!("no dispatches yet");
        return;
    }
    let window = match s.since_days {
        Some(d) => format!("last {d} days"),
        None => "all time".into(),
    };
    println!("{window}");
    println!(
        "  {} dispatches across {} task(s), {} still running",
        s.attempts, s.tasks_touched, s.live
    );

    let outcomes: Vec<String> = s.outcomes.iter().map(|(k, v)| format!("{v} {k}")).collect();
    if !outcomes.is_empty() {
        println!("  finished: {}", outcomes.join(", "));
    }
    if let Some(rate) = s.completion_rate {
        println!("  {:.0}% of finished attempts ended in done", rate * 100.0);
    }
    if let (Some(med), Some(max)) = (s.median_minutes, s.longest_minutes) {
        println!("  {med} min median, {max} min longest");
    }
    // Where it landed, all four categories and the losses among them (gh#228)
    // — off the same segments the two viewports draw, so the CLI cannot come
    // to call a rejected pull request something else.
    if s.landing.total() > 0 {
        let landed: Vec<String> = s
            .landing
            .segments()
            .iter()
            .map(|seg| format!("{} {}", seg.count, seg.label))
            .collect();
        println!("  landed: {}", landed.join(", "));
    }
    if let Some(note) = s.landing.in_flight_note() {
        println!("  {note}");
    }
    if s.friction.retried_tasks > 0 {
        println!(
            "  {} task(s) needed more than one go",
            s.friction.retried_tasks
        );
    }
    if s.friction.early_settles > 0 {
        println!(
            "  {} attempt(s) called finished while their agent was still working",
            s.friction.early_settles
        );
    }
    if s.agent_dispatched > 0 {
        println!(
            "  {} released by an agent rather than by you",
            s.agent_dispatched
        );
    }
    // Never a total without the coverage beside it: a figure summed from two
    // of five attempts is not the window's tokens, and saying so in the same
    // line is the only thing that keeps it from being read as if it were.
    if s.has_tokens() {
        println!(
            "  {} tokens ({} in, {} cached, {} out) across {}/{} attempt(s) that reported",
            human_tokens(s.tokens.total()),
            human_tokens(s.tokens.input_tokens),
            human_tokens(s.tokens.cache_read_tokens),
            human_tokens(s.tokens.output_tokens),
            s.attempts_with_tokens,
            s.attempts,
        );
    } else {
        println!("  no attempt in this window reported token usage");
    }
    // The other meter (gh#271), and the one thing the tokens above cannot say:
    // whether the work is shaped to fit inside one agent's context. Silent
    // when nothing reported — unlike the tokens, which say so out loud,
    // because a board of harnesses that meter no window would repeat that
    // sentence for ever.
    if s.context.is_reported() {
        let peak = s
            .context
            .peak_percent
            .map(|p| format!(", fullest {p}%"))
            .unwrap_or_default();
        println!(
            "  {}/{} attempt(s) that reported context ended near their compaction point{peak}",
            s.context.near_compaction, s.context.attempts_reported,
        );
    }
    // What that would cost at public API list prices (gh#182/gh#426) — and,
    // when the rates could not
    // price everything, what the figure leaves out. Never a bare total: a
    // number that silently dropped a model is the one failure this half of the
    // page is designed against.
    match &s.spend {
        None => {
            println!("  no model rates configured, so nothing is priced (see `[defaults.rates]`)")
        }
        Some(spend) if spend.has_price() => {
            println!(
                "  {} list-price API estimate ({}, from {}/{} attempts that reported usage; not a bill)",
                human_usd(spend.list_price),
                spend.rates.provenance(),
                s.attempts_with_tokens,
                s.attempts,
            );
            for row in spend.by_model.iter().take(4) {
                println!(
                    "    {} list-price API estimate · {} on {}",
                    human_usd(row.cost),
                    human_tokens(row.usage.total()),
                    row.label
                );
            }
            if !spend.is_complete() {
                println!(
                    "    {} token(s) on {} model(s) with no rate, and so not in that total: {}",
                    human_tokens(spend.unpriced_tokens),
                    spend.unpriced.len(),
                    spend
                        .unpriced
                        .iter()
                        .map(|t| t.label.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            // The second, separate fact: what the plans behind that work cost.
            // Never summed with the figure above — one is the board's, the
            // other is one person's.
            for account in spend.accounts.iter().filter(|a| a.plan.is_some()) {
                let plan = account.plan.as_ref().expect("filtered");
                let name = plan.label.as_deref().unwrap_or("subscription");
                match (account.plan_in_window, account.subsidy()) {
                    (Some(window), Some(ratio)) => println!(
                        "    {}: {} list-price API estimate against {} of {} ({name}, {}/month) — {:.0}%",
                        account.label,
                        human_usd(account.list_price),
                        human_usd(window),
                        s.window_label(),
                        human_usd(plan.monthly),
                        ratio * 100.0,
                    ),
                    _ => println!(
                        "    {}: {} list-price API estimate, on a {name} at {}/month",
                        account.label,
                        human_usd(account.list_price),
                        human_usd(plan.monthly),
                    ),
                }
            }
        }
        // Rates configured, and still no price: every metered model was one
        // the table has never heard of. `spend_label` is the sentence for it.
        Some(_) => println!("  {}", s.spend_label()),
    }
    for line in agent_usage_lines(s, 4) {
        println!("{line}");
    }
    let line = |label: &str, m: &BTreeMap<String, usize>| {
        if m.len() > 1 {
            let mut v: Vec<_> = m.iter().collect();
            v.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
            println!(
                "  by {label}: {}",
                v.iter()
                    .map(|(k, n)| format!("{k} {n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    };
    line("workspace", &s.by_workspace);
    line("runtime", &s.by_runtime);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use comet_proto::view::rates::Usd;

    fn task(id: &str, attempts: Vec<Attempt>) -> Task {
        Task {
            id: id.into(),
            source: Source::Linear,
            source_id: "u".into(),
            identifier: id.into(),
            title: "t".into(),
            body: None,
            url: "u".into(),
            labels: vec![],
            state: BoardState::Ready,
            source_state: None,
            upstream: UpstreamState::Unstarted,
            local_done: false,
            pr_url: None,
            pr_number: None,
            pr_open: false,
            pr_merged: false,
            pr_mergeable: None,
            pr_base_ref: None,
            pr_head_ref: None,
            pr_stack: None,
            pr_changes_requested: None,
            updated_at: crate::db::now(),
            synced_at: String::new(),
            attempts,
        }
    }

    fn attempt(
        minutes_ago: i64,
        ran_for: i64,
        outcome: Option<Outcome>,
        by: Option<&str>,
    ) -> Attempt {
        attempt_by(minutes_ago, ran_for, outcome, by, None)
    }

    /// `by` is a parent task id, `pane` the pane the dispatch came from —
    /// either one makes it an agent's, and a driving chat only ever has the
    /// second.
    fn attempt_by(
        minutes_ago: i64,
        ran_for: i64,
        outcome: Option<Outcome>,
        by: Option<&str>,
        pane: Option<&str>,
    ) -> Attempt {
        let start = chrono::Utc::now() - chrono::Duration::minutes(minutes_ago);
        Attempt {
            id: 0,
            task_id: String::new(),
            pane_id: None,
            branch: None,
            started_at: crate::db::rfc3339(start),
            ended_at: outcome
                .map(|_| crate::db::rfc3339(start + chrono::Duration::minutes(ran_for))),
            outcome,
            dispatched_by: by.map(str::to_string),
            dispatched_by_pane: pane.map(str::to_string),
            repo_path: Some("/repo/r".into()),
            ..crate::model::tests::blank_attempt()
        }
    }

    fn usage(input: u64, output: u64, read: u64, write: u64) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: read,
            cache_creation_tokens: write,
        }
    }

    #[test]
    fn an_empty_board_reports_nothing_rather_than_dividing_by_zero() {
        let s = gather(&[], None);
        assert_eq!(s.attempts, 0);
        assert_eq!(s.completion_rate, None);
        assert_eq!(s.median_minutes, None);
    }

    /// The furniture bit (gh#434) is all-time, never windowed: a board idle
    /// for a quiet week is still the org's board, and a board that only ever
    /// collected rows never stops being furniture.
    #[test]
    fn dispatch_evidence_ignores_the_window() {
        // An attempt well outside a 7-day window…
        let worked = task(
            "a",
            vec![attempt(60 * 24 * 30, 10, Some(Outcome::Done), None)],
        );
        let s = gather(&[worked], Some(7));
        assert_eq!(s.attempts, 0, "the window is empty");
        assert!(s.dispatched, "…is still dispatch evidence");
        // Rows with no attempts at all are furniture, whatever the window.
        let collected = task("b", vec![]);
        let s = gather(&[collected], None);
        assert!(!s.dispatched);
    }

    #[test]
    fn duration_is_a_median_not_a_mean() {
        // One agent left running overnight would drag a mean anywhere.
        let t = task(
            "a",
            vec![
                attempt(600, 10, Some(Outcome::Done), None),
                attempt(500, 12, Some(Outcome::Done), None),
                attempt(400, 900, Some(Outcome::Done), None),
            ],
        );
        let s = gather(&[t], None);
        assert_eq!(s.median_minutes, Some(12));
        assert_eq!(s.longest_minutes, Some(900));
    }

    #[test]
    fn completion_counts_only_attempts_that_ended() {
        let t = task(
            "a",
            vec![
                attempt(60, 10, Some(Outcome::Done), None),
                attempt(50, 5, Some(Outcome::Failed), None),
                attempt(10, 0, None, None), // still running
            ],
        );
        let s = gather(&[t], None);
        assert_eq!(s.live, 1);
        assert_eq!(
            s.completion_rate,
            Some(0.5),
            "the live one is not a failure"
        );
    }

    #[test]
    fn a_retry_is_counted_against_the_task_not_the_attempt() {
        let tasks = vec![
            task(
                "a",
                vec![
                    attempt(60, 5, Some(Outcome::Cancelled), None),
                    attempt(50, 5, Some(Outcome::Done), None),
                ],
            ),
            task("b", vec![attempt(40, 5, Some(Outcome::Done), None)]),
        ];
        let s = gather(&tasks, None);
        assert_eq!(s.friction.retried_tasks, 1);
        assert_eq!(s.tasks_touched, 2);
        assert_eq!(s.attempts, 3);
    }

    #[test]
    fn provenance_shows_how_much_the_herd_released_itself() {
        let t = task(
            "a",
            vec![
                attempt(60, 5, Some(Outcome::Done), Some("linear:LIN-1")),
                attempt(50, 5, Some(Outcome::Done), None),
            ],
        );
        assert_eq!(gather(&[t], None).agent_dispatched, 1);
    }

    /// The number used to be structurally 0: it counted only agents the board
    /// had dispatched, and those are not the ones that dispatch. An
    /// driving chat has a pane and no task, and it still counts (AGE-24).
    #[test]
    fn a_driving_chats_releases_are_counted_too() {
        let t = task(
            "a",
            vec![
                attempt_by(60, 5, Some(Outcome::Done), None, Some("w1:p3")),
                attempt_by(
                    55,
                    5,
                    Some(Outcome::Done),
                    Some("linear:LIN-1"),
                    Some("w2:p1"),
                ),
                attempt_by(50, 5, Some(Outcome::Done), None, None),
            ],
        );
        assert_eq!(gather(&[t], None).agent_dispatched, 2);
    }

    #[test]
    fn a_window_excludes_older_attempts() {
        let t = task(
            "a",
            vec![
                attempt(60 * 24 * 10, 5, Some(Outcome::Done), None), // 10 days ago
                attempt(30, 5, Some(Outcome::Done), None),
            ],
        );
        assert_eq!(gather(std::slice::from_ref(&t), None).attempts, 2);
        assert_eq!(gather(&[t], Some(7)).attempts, 1);
    }

    // -- the dashboard's series (gh#143) ------------------------------------

    /// The local day an attempt started `mins_ago` minutes back, as the bucket
    /// key `day_series` builds. Buckets are LOCAL dates, so "10 minutes ago" is
    /// only "today" until 00:10 — these tests were red in CI for the first
    /// twenty minutes of every UTC day until they asked this instead of
    /// assuming (found 2026-08-09 00:08 UTC).
    fn bucket_of(mins_ago: i64) -> String {
        (chrono::Local::now() - chrono::Duration::minutes(mins_ago))
            .date_naive()
            .format("%Y-%m-%d")
            .to_string()
    }

    #[test]
    fn a_window_draws_a_bar_for_every_day_including_the_quiet_ones() {
        // Two dispatches today and nothing for the six days before it: the
        // chart still has seven bars, because a gap that is simply absent
        // reads as data the board failed to record.
        let tasks = vec![task(
            "t1",
            vec![
                attempt(10, 5, Some(Outcome::Done), None),
                attempt(20, 5, Some(Outcome::Cancelled), None),
            ],
        )];
        let mut s = gather(&tasks, Some(7));
        assert_eq!(s.daily.len(), 7);
        // Which local day each attempt lands in is not the point and must not
        // be assumed: at 00:05 the two are on opposite sides of midnight. The
        // properties are that the window has a bar per day, that the dispatches
        // are in the days they actually happened, and that every other day is
        // present and empty.
        let done_day = bucket_of(10);
        let cancelled_day = bucket_of(20);
        let bar = |date: &str| {
            s.daily
                .iter()
                .find(|d| d.date == date)
                .unwrap_or_else(|| panic!("{date} is in the window"))
        };
        assert_eq!(bar(&done_day).done, 1, "only the one that ended done");
        assert!(bar(&done_day).dispatches >= 1);
        assert!(bar(&cancelled_day).dispatches >= 1);
        assert_eq!(s.daily.iter().map(|d| d.dispatches).sum::<usize>(), 2);
        assert_eq!(s.daily.iter().map(|d| d.done).sum::<usize>(), 1);
        assert!(
            s.daily
                .iter()
                .filter(|d| d.date != done_day && d.date != cancelled_day)
                .all(|d| d.dispatches == 0),
            "every other day is present and empty"
        );

        // And what the chart makes of that (gh#226). A bucket per day was only
        // half the promise: the other half is that a day with nothing in it is
        // *drawn*, at zero height with a dash where its figure would be.
        // Neither attempt reported tokens, so here every column is a quiet one.
        let columns = comet_proto::view::stats::day_columns(&s.daily, &s.daily_tokens);
        assert_eq!(columns.len(), 7);
        assert!(columns.iter().all(|c| c.is_quiet() && c.value == "—"));
        // A quiet bar still says what ran under it — the count the bars gave up
        // is in the caption, and no dispatch is lost on the way there.
        assert!(
            columns
                .iter()
                .all(|c| c.caption.ends_with(&format!(" · {}", c.dispatches))),
            "{:?}",
            columns.iter().map(|c| &c.caption).collect::<Vec<_>>()
        );
        assert_eq!(columns.iter().map(|c| c.dispatches).sum::<usize>(), 2);
        // Give one day some tokens and it is the only bar with height — the six
        // around it stay present at zero rather than going missing, which is
        // what made a week's work read as one lonely bar.
        s.daily_tokens
            .iter_mut()
            .filter(|d| d.date == done_day)
            .for_each(|d| d.usage = usage(90_000, 10_000, 0, 0));
        let drawn = comet_proto::view::stats::day_columns(&s.daily, &s.daily_tokens);
        let busy: Vec<&comet_proto::view::stats::DayColumn> =
            drawn.iter().filter(|c| !c.is_quiet()).collect();
        assert_eq!(busy.len(), 1);
        assert_eq!(busy[0].value, "100k");
        assert_eq!(busy[0].fraction, 1.0);
        assert_eq!(
            drawn.iter().filter(|c| c.fraction == 0.0).count(),
            6,
            "the quiet days are present at zero height, not absent"
        );
    }

    #[test]
    fn the_hour_histogram_has_a_slot_for_every_hour() {
        let tasks = vec![task("t1", vec![attempt(30, 5, Some(Outcome::Done), None)])];
        let s = gather(&tasks, None);
        assert_eq!(s.hour_of_day.len(), 24);
        assert_eq!(s.hour_of_day.iter().sum::<usize>(), 1);
    }

    #[test]
    fn when_the_work_ran_is_crossed_with_where_it_went() {
        // Two spaces, two dispatches an hour apart (gh#179). The hour margin
        // and the workspace margin each hold half the story; the crossing is
        // what says WHICH space was the late one.
        let mut early = attempt(120, 5, Some(Outcome::Done), None);
        early.workspace = "comet".into();
        let mut late = attempt(30, 5, Some(Outcome::Done), None);
        late.workspace = "edge".into();
        let early_hour = started_local(&early).map(|t| {
            use chrono::Timelike as _;
            t.hour() as usize
        });
        let late_hour = started_local(&late).map(|t| {
            use chrono::Timelike as _;
            t.hour() as usize
        });
        let (Some(early_hour), Some(late_hour)) = (early_hour, late_hour) else {
            panic!("both attempts parse");
        };

        let s = gather(&[task("t1", vec![early, late])], None);
        let comet = s.hours_by_workspace.get("comet").expect("a row per space");
        let edge = s.hours_by_workspace.get("edge").expect("a row per space");
        assert_eq!(comet.len(), 24, "a slot for every hour, like the margin");
        assert_eq!(comet[early_hour], 1);
        assert_eq!(edge[late_hour], 1);
        // The crossing sums back to both margins it replaced — a grid that
        // disagreed with the histogram beside it would be two answers.
        assert_eq!(comet.iter().sum::<usize>(), s.by_workspace["comet"]);
        let crossed: usize = s.hours_by_workspace.values().flatten().sum();
        assert_eq!(crossed, s.hour_of_day.iter().sum::<usize>());
    }

    #[test]
    fn where_the_work_landed_is_counted_per_task_not_per_attempt() {
        // Three goes at one issue produce one pull request; counting attempts
        // would report the same merge three times.
        let mut merged = task(
            "t1",
            vec![
                attempt(60, 5, Some(Outcome::Failed), None),
                attempt(50, 5, Some(Outcome::Failed), None),
                attempt(40, 5, Some(Outcome::Done), None),
            ],
        );
        merged.pr_number = Some(7);
        merged.pr_merged = true;
        let mut in_review = task("t2", vec![attempt(30, 5, Some(Outcome::Done), None)]);
        in_review.pr_number = Some(8);
        in_review.pr_open = true;
        let mut abandoned = task("t3", vec![attempt(20, 5, Some(Outcome::Cancelled), None)]);
        abandoned.pr_number = Some(9);
        let no_pr = task("t4", vec![attempt(10, 5, Some(Outcome::Done), None)]);

        let s = gather(&[merged, in_review, abandoned, no_pr], None);
        assert_eq!(s.landing.merged, 1);
        assert_eq!(s.landing.open, 1);
        assert_eq!(s.landing.closed_unmerged, 1);
        assert_eq!(s.landing.no_pr, 1);
        assert_eq!(s.landing.in_flight, 0);
        assert_eq!(s.landing.total(), s.tasks_touched);
    }

    #[test]
    fn an_agent_still_typing_is_not_an_agent_that_came_back_empty() {
        // gh#228: both tasks have no pull request, and keyed on that alone
        // they are one number — which is how a loss comes to read as work in
        // flight. The attempts are what tells them apart.
        let settled_empty = task("t1", vec![attempt(60, 5, Some(Outcome::Done), None)]);
        let still_going = task("t2", vec![attempt(10, 0, None, None)]);
        // A second go still running on a task whose first go already ended:
        // the task has not landed anywhere, so it is not a loss yet either.
        let retrying = task(
            "t3",
            vec![
                attempt(90, 5, Some(Outcome::Failed), None),
                attempt(5, 0, None, None),
            ],
        );

        let s = gather(&[settled_empty, still_going, retrying], None);
        assert_eq!(s.landing.no_pr, 1, "only the one that came back empty");
        assert_eq!(s.landing.in_flight, 2);
        // The bar is over what landed; the rest is a caption beside it.
        assert_eq!(s.landing.total(), 1);
        assert_eq!(s.landing.touched(), s.tasks_touched);

        // A merged pull request is a merge whatever a retry is doing under it.
        let mut merged_and_retrying = task(
            "t4",
            vec![
                attempt(90, 5, Some(Outcome::Done), None),
                attempt(5, 0, None, None),
            ],
        );
        merged_and_retrying.pr_number = Some(11);
        merged_and_retrying.pr_merged = true;
        let s = gather(&[merged_and_retrying], None);
        assert_eq!(s.landing.merged, 1);
        assert_eq!(s.landing.in_flight, 0);
    }

    #[test]
    fn friction_counts_transitions_and_the_boards_own_misjudgements() {
        let mut a = attempt(60, 30, Some(Outcome::Done), None);
        a.blocked_count = 3;
        a.reopened = 1;
        a.overrun_warned_at = Some(crate::db::now());
        let mut b = attempt(50, 10, Some(Outcome::Done), None);
        b.blocked_count = 1;
        let s = gather(&[task("t1", vec![a, b])], None);
        assert_eq!(s.friction.blocked_entries, 4);
        assert_eq!(s.friction.early_settles, 1);
        assert_eq!(s.friction.overruns, 1);
        assert_eq!(s.friction.retried_tasks, 1, "two attempts on one task");
        assert!(!s.friction.is_clean());
    }

    #[test]
    fn a_dispatch_that_named_no_slot_is_still_attributed_to_a_payer() {
        // The billing guard exists for exactly this row: work that quietly
        // spent the box owner's subscription must not read as unattributed.
        let mut paid = attempt(30, 5, Some(Outcome::Done), None);
        paid.billed_to = Some("brede@tally.no".into());
        let free = attempt(20, 5, Some(Outcome::Done), None);
        let s = gather(&[task("t1", vec![paid, free])], None);
        assert_eq!(s.by_account.get("brede@tally.no"), Some(&1));
        assert_eq!(s.by_account.get("the box's own login"), Some(&1));
    }

    // -- tokens (gh#151) ----------------------------------------------------

    #[test]
    fn a_window_reports_what_share_of_its_attempts_it_could_account_for() {
        // The honesty line. Two of three attempts reported; the third ran
        // before the board recorded tokens. The total is the two, and the page
        // says so rather than presenting it as the window's spend.
        let mut metered = attempt(60, 5, Some(Outcome::Done), None);
        metered.tokens = Some(usage(1_000, 200, 40_000, 3_000));
        let mut also = attempt(50, 5, Some(Outcome::Done), None);
        also.tokens = Some(usage(500, 100, 0, 0));
        let silent = attempt(40, 5, Some(Outcome::Done), None);

        let s = gather(&[task("t1", vec![metered, also, silent])], None);
        assert_eq!(s.attempts_with_tokens, 2);
        assert_eq!(s.attempts, 3);
        assert_eq!(s.token_coverage, Some(2.0 / 3.0));
        assert_eq!(s.tokens.input_tokens, 1_500);
        assert_eq!(s.tokens.cache_read_tokens, 40_000);
        assert_eq!(s.tokens.total(), 44_800);
        assert!(s.has_tokens());
    }

    /// A run that reported nothing must never be summed in as a zero — and a
    /// window of only such runs has no total to show at all, only a 0%.
    #[test]
    fn attempts_that_reported_nothing_are_absent_from_the_total_not_zero_in_it() {
        let s = gather(
            &[task(
                "t1",
                vec![
                    attempt(60, 5, Some(Outcome::Done), None),
                    attempt(50, 5, Some(Outcome::Done), None),
                ],
            )],
            None,
        );
        assert!(!s.has_tokens());
        assert!(s.tokens.is_zero());
        assert_eq!(s.attempts_with_tokens, 0);
        assert_eq!(
            s.token_coverage,
            Some(0.0),
            "attempts ran and none reported — a real 0%, worth saying"
        );
        // Nothing ran at all is a different fact, and has no share.
        assert_eq!(gather(&[], None).token_coverage, None);
    }

    // -- context fullness (gh#271) -----------------------------------------

    /// The page's second meter, on the first one's terms: the share is over
    /// the attempts that reported a window, never over the window, because a
    /// board running one harness that meters nothing would otherwise look like
    /// a board with no context pressure.
    #[test]
    fn the_context_line_counts_only_the_attempts_that_reported_a_window() {
        let full = |used: u64, at: Option<u64>| comet_proto::ContextUsage {
            used_tokens: used,
            max_tokens: 200_000,
            compact_at_tokens: at,
        };
        // Past the harness's own threshold…
        let mut compacting = attempt(60, 5, Some(Outcome::Done), None);
        compacting.context = Some(full(180_000, Some(167_000)));
        // …past 90% with no threshold named (a codex attempt)…
        let mut ratio = attempt(55, 5, Some(Outcome::Done), None);
        ratio.context = Some(full(185_000, None));
        // …comfortable…
        let mut roomy = attempt(50, 5, Some(Outcome::Done), None);
        roomy.context = Some(full(30_000, Some(167_000)));
        // …and one whose harness metered no window at all.
        let silent = attempt(40, 5, Some(Outcome::Done), None);

        let s = gather(&[task("t1", vec![compacting, ratio, roomy, silent])], None);
        assert!(s.context.is_reported());
        assert_eq!(s.context.attempts_reported, 3, "the silent one is not one");
        assert_eq!(s.context.near_compaction, 2);
        assert_eq!(s.context.peak_percent, Some(93));
        assert_eq!(s.attempts, 4, "and all four are still dispatches");
    }

    /// A board whose harnesses meter no window says nothing rather than
    /// reporting no pressure — the two are not the same claim.
    #[test]
    fn a_window_nobody_measured_reports_no_pressure_rather_than_none_found() {
        let s = gather(
            &[task("t1", vec![attempt(60, 5, Some(Outcome::Done), None)])],
            None,
        );
        assert!(!s.context.is_reported());
        assert_eq!(s.context.peak_percent, None, "never Some(0)");
        assert_eq!(s.context.near_compaction, 0);
    }

    #[test]
    fn tokens_are_split_by_the_model_the_run_named_and_by_the_runtime() {
        let mut opus = attempt(60, 5, Some(Outcome::Done), None);
        opus.tokens = Some(usage(100, 10, 0, 0));
        opus.model = Some("claude-opus-5".into());
        let mut opus_again = attempt(55, 5, Some(Outcome::Done), None);
        opus_again.tokens = Some(usage(50, 5, 0, 0));
        opus_again.model = Some("claude-opus-5".into());
        // A run whose journal never said. It still counts — dropping it would
        // leave a per-model table that does not add up to the headline.
        let mut anonymous = attempt(50, 5, Some(Outcome::Done), None);
        anonymous.tokens = Some(usage(7, 1, 0, 0));
        anonymous.runtime = "codex".into();

        let s = gather(&[task("t1", vec![opus, opus_again, anonymous])], None);
        assert_eq!(s.tokens_by_model["claude-opus-5"].total(), 165);
        assert_eq!(s.tokens_by_model["unnamed model"].total(), 8);
        assert_eq!(
            s.tokens_by_model.values().map(|u| u.total()).sum::<u64>(),
            s.tokens.total(),
            "the breakdown accounts for the headline"
        );
        assert_eq!(s.tokens_by_runtime["claude-code"].total(), 165);
        assert_eq!(s.tokens_by_runtime["codex"].total(), 8);
    }

    #[test]
    fn the_token_series_covers_the_same_days_as_the_dispatch_series() {
        // Index-aligned by construction: the page draws one under the other.
        let mut spent = attempt(10, 5, Some(Outcome::Done), None);
        spent.tokens = Some(usage(1_000, 100, 0, 0));
        let s = gather(&[task("t1", vec![spent])], Some(7));
        assert_eq!(s.daily.len(), 7);
        assert_eq!(s.daily_tokens.len(), 7);
        let dates: Vec<&str> = s.daily.iter().map(|d| d.date.as_str()).collect();
        let token_dates: Vec<&str> = s.daily_tokens.iter().map(|d| d.date.as_str()).collect();
        assert_eq!(dates, token_dates);
        let day = bucket_of(10);
        let spent = s
            .daily_tokens
            .iter()
            .find(|d| d.date == day)
            .expect("the day is in the window");
        assert_eq!(spent.usage.total(), 1_100);
        assert!(
            s.daily_tokens
                .iter()
                .filter(|d| d.date != day)
                .all(|d| d.usage.is_zero()),
            "quiet days are present with zeroes, like the bars above them"
        );
    }

    #[test]
    fn a_window_excludes_the_tokens_of_the_attempts_it_excludes() {
        let mut old = attempt(60 * 24 * 10, 5, Some(Outcome::Done), None);
        old.tokens = Some(usage(9_000, 900, 0, 0));
        let mut recent = attempt(30, 5, Some(Outcome::Done), None);
        recent.tokens = Some(usage(100, 10, 0, 0));
        let t = task("t1", vec![old, recent]);
        assert_eq!(
            gather(std::slice::from_ref(&t), None).tokens.total(),
            10_010
        );
        assert_eq!(gather(&[t], Some(7)).tokens.total(), 110);
    }

    // -- spend (gh#182) -----------------------------------------------------

    /// A metered attempt, on a model, billed to somebody.
    fn spent(minutes_ago: i64, model: &str, billed_to: Option<&str>, usage: TokenUsage) -> Attempt {
        let mut a = attempt(minutes_ago, 5, Some(Outcome::Done), None);
        a.model = Some(model.into());
        a.billed_to = billed_to.map(str::to_string);
        a.tokens = Some(usage);
        a
    }

    #[test]
    fn a_window_gathered_without_rates_has_no_price_rather_than_a_free_one() {
        // The unpriced gather is not "everything was free" — it is a caller
        // that had no rates, and every surface says so in those words.
        let s = gather(
            &[task(
                "t1",
                vec![spent(30, "claude-opus-5", None, usage(1_000_000, 0, 0, 0))],
            )],
            Some(7),
        );
        assert_eq!(s.spend, None);
        assert!(!s.has_spend());
        assert_eq!(s.spend_label(), "rates not configured");
    }

    #[test]
    fn a_priced_window_totals_what_it_can_and_names_what_it_cannot() {
        let tasks = vec![task(
            "t1",
            vec![
                // Priced by the shipped table, in all four buckets.
                spent(
                    60,
                    "claude-opus-5",
                    Some("brede@tally.no"),
                    usage(10_000, 2_000, 1_000_000, 20_000),
                ),
                // And a model the table has never heard of.
                spent(
                    50,
                    "gpt-5.6-luna",
                    Some("brede@tally.no"),
                    usage(400, 100, 0, 500),
                ),
            ],
        )];
        let s = gather_priced(&tasks, Some(7), &crate::prices::Prices::builtin());
        let spend = s.spend.as_ref().expect("rates are configured");

        assert_eq!(spend.list_price, Usd::from_dollars(0.725));
        assert_eq!(spend.by_model.len(), 1);
        assert_eq!(spend.unpriced.len(), 1);
        assert_eq!(spend.unpriced[0].label, "gpt-5.6-luna");
        assert_eq!(spend.unpriced_tokens, 1_000);
        assert!(!spend.is_complete());
        assert!(s.has_spend());
        // The headline never stands alone while something is missing from it.
        assert!(s.spend_label().contains("unpriced"), "{}", s.spend_label());
    }

    #[test]
    fn a_mixed_model_attempt_is_priced_and_split_by_the_agents_that_ran_it() {
        let mut mixed = spent(30, "claude-opus-5", None, usage(2_000_000, 0, 0, 0));
        mixed.token_models = Some(vec![
            comet_proto::ModelTokenUsage {
                model: "claude-opus-5".into(),
                usage: usage(1_000_000, 0, 0, 0),
            },
            comet_proto::ModelTokenUsage {
                model: "claude-haiku-4-5".into(),
                usage: usage(1_000_000, 0, 0, 0),
            },
        ]);
        mixed.token_agents = Some(vec![
            comet_proto::AgentTokenUsage {
                agent: comet_proto::AgentKind::Main,
                name: None,
                model: "claude-opus-5".into(),
                usage: usage(1_000_000, 0, 0, 0),
            },
            comet_proto::AgentTokenUsage {
                agent: comet_proto::AgentKind::Subagent,
                name: Some("Explore".into()),
                model: "claude-haiku-4-5".into(),
                usage: usage(1_000_000, 0, 0, 0),
            },
        ]);

        let s = gather_priced(
            &[task("t1", vec![mixed])],
            Some(7),
            &crate::prices::Prices::builtin(),
        );
        assert_eq!(
            s.spend.as_ref().expect("priced").list_price,
            Usd::from_dollars(6.0),
            "one million Opus input tokens plus one million Haiku input tokens"
        );
        assert_eq!(s.tokens_by_model["claude-opus-5"].total(), 1_000_000);
        assert_eq!(s.tokens_by_model["claude-haiku-4-5"].total(), 1_000_000);
        let models = s
            .breakdown
            .iter()
            .find(|cut| cut.dimension == Dimension::Model)
            .expect("model breakdown");
        for (label, estimate) in [
            ("claude-opus-5", Usd::from_dollars(5.0)),
            ("claude-haiku-4-5", Usd::from_dollars(1.0)),
        ] {
            let row = models
                .rows
                .iter()
                .find(|row| row.label == label)
                .unwrap_or_else(|| panic!("actual model row {label}"));
            assert_eq!(row.dispatches, 1, "one attempt used {label}");
            assert_eq!(row.usage.total(), 1_000_000);
            assert_eq!(row.cost, Some(estimate));
        }
        assert_eq!(s.attempts_with_agent_usage, 1);
        assert_eq!(s.agent_usage.len(), 2);
        assert_eq!(s.agent_usage[0].label(), "Main · claude-opus-5");
        assert_eq!(
            s.agent_usage[0].list_price_api_estimate,
            Some(Usd::from_dollars(5.0))
        );
        assert_eq!(s.agent_usage[1].label(), "Explore · claude-haiku-4-5");
        assert_eq!(
            s.agent_usage[1].list_price_api_estimate,
            Some(Usd::from_dollars(1.0))
        );
        assert_eq!(s.token_coverage, Some(1.0));

        // `comet-board stats --json` serializes this value directly. Keep the
        // compatible money names, but never emit them without the response's
        // explicit estimate basis beside them.
        let json = serde_json::to_value(&s).expect("stats JSON");
        assert_eq!(json["pricingBasis"], "listPriceApiEstimate");
        assert!(json["spend"].get("listPrice").is_some());
        assert!(
            json["spend"]["byModel"]
                .as_array()
                .is_some_and(|rows| { rows.iter().all(|row| row.get("cost").is_some()) })
        );
        assert!(json["breakdown"].as_array().is_some_and(|cuts| {
            cuts.iter()
                .flat_map(|cut| cut["rows"].as_array())
                .any(|rows| rows.iter().any(|row| row.get("cost").is_some()))
        }));
    }

    #[test]
    fn a_model_split_that_does_not_reconcile_falls_back_to_the_attempt_model() {
        let mut attempt = spent(30, "claude-opus-5", None, usage(2_000_000, 0, 0, 0));
        attempt.token_models = Some(vec![comet_proto::ModelTokenUsage {
            model: "claude-haiku-4-5".into(),
            usage: usage(1_000_000, 0, 0, 0),
        }]);

        let s = gather_priced(
            &[task("t1", vec![attempt])],
            Some(7),
            &crate::prices::Prices::builtin(),
        );
        assert_eq!(s.tokens_by_model.len(), 1);
        assert_eq!(s.tokens_by_model["claude-opus-5"].total(), 2_000_000);
        assert_eq!(
            s.spend.as_ref().expect("priced").list_price,
            Usd::from_dollars(10.0),
            "a partial attribution may not make one million tokens disappear"
        );
    }

    #[test]
    fn agent_attribution_that_exceeds_the_attempt_total_is_not_reported_as_covered() {
        let mut attempt = spent(30, "claude-opus-5", None, usage(100, 10, 0, 0));
        attempt.token_agents = Some(vec![comet_proto::AgentTokenUsage {
            agent: comet_proto::AgentKind::Subagent,
            name: Some("Explore".into()),
            model: "claude-haiku-4-5".into(),
            usage: usage(130, 13, 0, 0),
        }]);

        let s = gather_priced(
            &[task("t1", vec![attempt])],
            Some(7),
            &crate::prices::Prices::builtin(),
        );
        assert_eq!(s.attempts_with_tokens, 1);
        assert_eq!(s.attempts_with_agent_usage, 0);
        assert!(s.agent_usage.is_empty());
    }

    #[test]
    fn cli_agent_rows_keep_an_all_unpriced_model_visible() {
        let mut attempt = spent(30, "unknown-main", None, usage(100, 10, 0, 0));
        attempt.token_agents = Some(vec![comet_proto::AgentTokenUsage {
            agent: comet_proto::AgentKind::Subagent,
            name: Some("Explore".into()),
            model: "unknown-research-model".into(),
            usage: usage(100, 10, 0, 0),
        }]);
        let s = gather_priced(
            &[task("t1", vec![attempt])],
            Some(7),
            &crate::prices::Prices::builtin(),
        );
        assert!(!s.spend.as_ref().expect("rates configured").has_price());

        let text = agent_usage_lines(&s, 4).join("\n");
        assert!(text.contains("by agent/model list-price API estimate"));
        assert!(text.contains("Explore · unknown-research-model"));
        assert!(text.contains("unpriced"));
    }

    #[test]
    fn tokens_are_split_by_payer_so_a_price_can_be_put_on_each() {
        let tasks = vec![task(
            "t1",
            vec![
                spent(
                    60,
                    "claude-opus-5",
                    Some("brede@tally.no"),
                    usage(1_000_000, 0, 0, 0),
                ),
                spent(
                    50,
                    "claude-haiku-4-5",
                    Some("ana@example.com"),
                    usage(1_000_000, 0, 0, 0),
                ),
                // A dispatch that named no slot ran on the box's own login,
                // which is a payer with a name rather than a blank.
                spent(40, "claude-opus-5", None, usage(200_000, 0, 0, 0)),
            ],
        )];
        let s = gather_priced(&tasks, Some(30), &crate::prices::Prices::builtin());

        assert_eq!(s.tokens_by_account["brede@tally.no"].total(), 1_000_000);
        assert_eq!(s.tokens_by_account[THE_BOX].total(), 200_000);
        assert_eq!(
            s.tokens_by_account.values().map(|u| u.total()).sum::<u64>(),
            s.tokens.total(),
            "the split accounts for the headline"
        );

        let spend = s.spend.as_ref().expect("rates are configured");
        let by = |who: &str| {
            spend
                .accounts
                .iter()
                .find(|a| a.label == who)
                .unwrap_or_else(|| panic!("{who}"))
                .list_price
        };
        // Each account is priced at the rates of the models IT ran — the whole
        // reason the split is kept by (payer, model) rather than by payer.
        assert_eq!(by("brede@tally.no"), Usd::from_dollars(5.0));
        assert_eq!(by("ana@example.com"), Usd::from_dollars(1.0));
        assert_eq!(by(THE_BOX), Usd::from_dollars(1.0));
        assert_eq!(
            spend.accounts.iter().map(|a| a.list_price).sum::<Usd>(),
            spend.list_price,
            "the account rows add up to the headline"
        );
    }

    /// The two facts stay two facts: what the board ran, and what somebody
    /// pays for the plan it ran on.
    #[test]
    fn a_configured_plan_is_reported_beside_the_price_and_never_inside_it() {
        let cfg: crate::config::RoutingConfig = toml::from_str(
            r#"
[account."8f2c1d0a7b6e4539"]
email = "brede@tally.no"
plan = "Claude Max 20x"
monthly_usd = 200
"#,
        )
        .expect("parses");
        let prices = crate::prices::Prices::from_config(&cfg);
        let tasks = vec![task(
            "t1",
            vec![spent(
                30,
                "claude-opus-5",
                Some("brede@tally.no"),
                usage(4_000_000, 0, 0, 0),
            )],
        )];
        let s = gather_priced(&tasks, Some(30), &prices);
        let spend = s.spend.as_ref().expect("rates are configured");
        let row = &spend.accounts[0];

        assert_eq!(row.list_price, Usd::from_dollars(20.0));
        assert_eq!(
            row.plan.as_ref().and_then(|p| p.label.clone()).as_deref(),
            Some("Claude Max 20x")
        );
        assert_eq!(row.plan_in_window, Some(Usd::from_dollars(200.0)));
        assert_eq!(row.subsidy(), Some(0.1), "a tenth of what the plan cost");
        // And the plan is nowhere in the board's own figure.
        assert_eq!(spend.list_price, Usd::from_dollars(20.0));
    }

    // -- the breakdown (gh#227) ----------------------------------------------

    fn cut(s: &Stats, dimension: Dimension) -> &Breakdown {
        s.cut(dimension)
            .unwrap_or_else(|| panic!("{dimension:?} is a dimension of this window"))
    }

    /// The dimension the shipped page could not ask about at all, and the one
    /// the card exists for: which model is the bill.
    #[test]
    fn the_window_is_cut_by_model_and_every_row_carries_its_money() {
        let tasks = vec![task(
            "t1",
            vec![
                spent(
                    60,
                    "claude-opus-5",
                    Some("brede@tally.no"),
                    usage(1_000_000, 0, 0, 0),
                ),
                spent(
                    50,
                    "claude-opus-5",
                    Some("brede@tally.no"),
                    usage(1_000_000, 0, 0, 0),
                ),
                spent(
                    40,
                    "claude-haiku-4-5",
                    Some("ana@example.com"),
                    usage(1_000_000, 0, 0, 0),
                ),
            ],
        )];
        let s = gather_priced(&tasks, Some(7), &crate::prices::Prices::builtin());
        let models = cut(&s, Dimension::Model);

        assert_eq!(models.ranking, Ranking::Spend);
        let labels: Vec<&str> = models.rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, ["claude-opus-5", "claude-haiku-4-5"]);
        assert_eq!(models.rows[0].dispatches, 2);
        assert_eq!(models.rows[0].usage.total(), 2_000_000);
        assert_eq!(models.rows[0].cost, Some(Usd::from_dollars(10.0)));
        assert_eq!(models.rows[1].cost, Some(Usd::from_dollars(1.0)));
        // The rows account for the headline they sit under.
        assert_eq!(
            models.rows.iter().filter_map(|r| r.cost).sum::<Usd>(),
            s.spend.as_ref().expect("priced").list_price
        );
        // A bar drawn against the biggest row's money, which is what it sorted
        // on — 10× the row under it.
        assert_eq!(models.share(&models.rows[0]), 1.0);
        assert_eq!(models.share(&models.rows[1]), 0.1);
    }

    /// The point of pricing per bucket rather than per board: a cut whose rows
    /// ran different models is priced at each row's own rates.
    #[test]
    fn a_cut_is_priced_at_the_rates_of_the_models_that_row_actually_ran() {
        let mut cheap = spent(60, "claude-haiku-4-5", None, usage(1_000_000, 0, 0, 0));
        cheap.runtime = "codex".into();
        cheap.workspace = "edge".into();
        let dear = spent(50, "claude-opus-5", None, usage(1_000_000, 0, 0, 0));
        let s = gather_priced(
            &[task("t1", vec![cheap, dear])],
            Some(7),
            &crate::prices::Prices::builtin(),
        );

        let runtimes = cut(&s, Dimension::Runtime);
        let by = |rows: &[BreakdownRow], label: &str| {
            rows.iter()
                .find(|r| r.label == label)
                .unwrap_or_else(|| panic!("{label}"))
                .cost
        };
        // Same tokens on both rows, five times the money on one of them. An
        // average rate would have said $3 twice.
        assert_eq!(
            by(&runtimes.rows, "claude-code"),
            Some(Usd::from_dollars(5.0))
        );
        assert_eq!(by(&runtimes.rows, "codex"), Some(Usd::from_dollars(1.0)));
        let spaces = cut(&s, Dimension::Space);
        assert_eq!(by(&spaces.rows, "edge"), Some(Usd::from_dollars(1.0)));
    }

    /// gh#359: the whole path, from an attempt on a model nobody has a rate
    /// for to the row the card draws for it. Tokens are a fact the board knows
    /// exactly — only the money is unknown, and only the money says so.
    #[test]
    fn a_model_the_table_never_heard_of_keeps_its_row_and_its_tokens() {
        let tasks = vec![task(
            "t1",
            vec![
                spent(60, "claude-opus-5", None, usage(1_000_000, 0, 0, 0)),
                spent(50, "gpt-5.6-luna", None, usage(2_900_000, 0, 0, 0)),
            ],
        )];
        let s = gather_priced(&tasks, Some(7), &crate::prices::Prices::builtin());
        let models = cut(&s, Dimension::Model);

        // Present, and under the row that has a price rather than dropped or
        // ranked among them at a zero it never spent.
        let labels: Vec<&str> = models.rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, ["claude-opus-5", "gpt-5.6-luna"]);
        let luna = &models.rows[1];
        assert_eq!(luna.usage.total(), 2_900_000, "the board counted these");
        assert_eq!(luna.dispatches, 1);
        assert!(luna.is_unpriced());
        assert_eq!(luna.price_label(), comet_proto::view::stats::UNPRICED);
        assert_eq!(models.rows[0].price_label(), "$5.00");

        // And the footer's reconciliation is the same tokens said once more:
        // what the row could not price is what the headline leaves out.
        let spend = s.spend.as_ref().expect("priced");
        assert_eq!(spend.unpriced_tokens, luna.usage.total());
        assert_eq!(spend.list_price, Usd::from_dollars(5.0));
        let named: Vec<&str> = spend.unpriced.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(named, ["gpt-5.6-luna"]);
    }

    #[test]
    fn a_dimension_this_window_has_nothing_under_is_not_offered_at_all() {
        // Nothing ran: no cut has a row, so the card has no toggle to draw.
        assert!(gather(&[], Some(7)).breakdown.is_empty());

        // And a window that ran: every axis an attempt carries is a dimension,
        // and each is present exactly once.
        let s = gather(
            &[task("t1", vec![attempt(30, 5, Some(Outcome::Done), None)])],
            Some(7),
        );
        let offered: Vec<Dimension> = s.breakdown.iter().map(|b| b.dimension).collect();
        assert_eq!(offered, Dimension::ALL.to_vec(), "in the toggle's order");
        assert_eq!(cut(&s, Dimension::Tracker).rows[0].label, "linear");
        assert_eq!(cut(&s, Dimension::Account).rows[0].label, THE_BOX);
    }

    /// A dispatch belongs under its model whether or not the harness metered
    /// it — and a run that never said gets the one name every other split
    /// gives it, rather than vanishing out of the count.
    #[test]
    fn an_unmetered_dispatch_is_still_a_row_and_is_ranked_on_what_is_left() {
        let mut named = attempt(60, 5, Some(Outcome::Done), None);
        named.model = Some("claude-opus-5".into());
        let silent = attempt(50, 5, Some(Outcome::Done), None);
        let s = gather(&[task("t1", vec![named, silent])], Some(7));

        let models = cut(&s, Dimension::Model);
        assert_eq!(models.ranking, Ranking::Dispatches, "nothing to rank on");
        let labels: Vec<&str> = models.rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, ["claude-opus-5", UNNAMED_MODEL]);
        assert!(
            !models.is_priced(),
            "an unpriced gather has no money column"
        );
        assert!(models.rows.iter().all(|r| r.dispatches == 1));
    }

    #[test]
    fn the_ninth_decile_never_claims_a_duration_nothing_took() {
        // Four attempts: the p90 is the longest of them, not an interpolation
        // between two that would name a number no attempt ever ran for.
        let tasks = vec![task(
            "t1",
            vec![
                attempt(100, 10, Some(Outcome::Done), None),
                attempt(90, 20, Some(Outcome::Done), None),
                attempt(80, 30, Some(Outcome::Done), None),
                attempt(70, 40, Some(Outcome::Done), None),
            ],
        )];
        let s = gather(&tasks, None);
        assert_eq!(s.p90_minutes, Some(40));
        assert_eq!(s.longest_minutes, Some(40));
        assert_eq!(s.total_minutes, 100);
    }
}
