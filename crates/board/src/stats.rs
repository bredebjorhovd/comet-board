//! What the board knows about its own throughput.
//!
//! Every attempt is already recorded with when it started, when it ended, how
//! it ended, which runtime ran it, which workspace it went to, and whether an
//! agent or the operator released it. That is enough to answer the only
//! question that matters about delegating work: whether it is actually
//! finishing, and how often it has to be done twice.
//!
//! Deliberately descriptive. It reports what happened; it does not grade it.

use crate::config::Paths;
use crate::db::Db;
use crate::log::Logger;
use crate::model::{Attempt, Outcome, Task};
use anyhow::Result;
use std::collections::BTreeMap;
use std::sync::Arc;

/// The shape lives in proto so the viewports can deserialize a `BoardStats`
/// reply without linking this crate (the [`crate::runtime::RuntimeOption`]
/// split, for the same reason). What it *contains* is this module's.
pub use comet_proto::TokenUsage;
pub use comet_proto::view::stats::{
    BoardStats as Stats, DayBucket, Friction, Landing, TokenDay, human_tokens,
};

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

pub fn gather(tasks: &[Task], since_days: Option<i64>) -> Stats {
    let attempts: Vec<(&Task, &Attempt)> = tasks
        .iter()
        .flat_map(|t| t.attempts.iter().map(move |a| (t, a)))
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
    let mut hour_of_day = vec![0usize; 24];
    let mut live = 0;
    let mut agent_dispatched = 0;
    let mut friction = Friction::default();
    // Tokens are summed only over attempts that reported any. An attempt with
    // no record contributes nothing and is counted out of the coverage below,
    // rather than adding a zero that would quietly deflate every total.
    let mut tokens = TokenUsage::default();
    let mut attempts_with_tokens = 0usize;
    let mut tokens_by_model: BTreeMap<String, TokenUsage> = BTreeMap::new();
    let mut tokens_by_runtime: BTreeMap<String, TokenUsage> = BTreeMap::new();

    for (task, a) in &attempts {
        match a.outcome {
            Some(o) => *outcomes.entry(o.as_str().to_string()).or_default() += 1,
            None => live += 1,
        }
        *by_workspace.entry(a.workspace.clone()).or_default() += 1;
        *by_runtime.entry(a.runtime.clone()).or_default() += 1;
        *by_source
            .entry(task.source.as_str().to_string())
            .or_default() += 1;
        // Whose subscription it spent (gh#101). A dispatch that named no slot
        // ran on the box's own login, and saying so is the point of the row —
        // "unattributed" would hide exactly the case the guard exists for.
        let payer = a
            .billed_to
            .clone()
            .unwrap_or_else(|| "the box's own login".to_string());
        *by_account.entry(payer).or_default() += 1;
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
            let model = a.model.clone().unwrap_or_else(|| "unnamed model".into());
            tokens_by_model.entry(model).or_default().add(usage);
            tokens_by_runtime
                .entry(a.runtime.clone())
                .or_default()
                .add(usage);
        }
        if let Some(start) = started_local(a) {
            use chrono::Timelike as _;
            hour_of_day[start.hour() as usize] += 1;
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
    let mut landing = Landing::default();
    for task in tasks.iter().filter(|t| touched.contains(t.id.as_str())) {
        if task.pr_merged {
            landing.merged += 1;
        } else if task.pr_open {
            landing.open += 1;
        } else if task.pr_number.is_some() {
            landing.closed_unmerged += 1;
        } else {
            landing.no_pr += 1;
        }
    }

    let calendar = day_range(&days, since_days);

    Stats {
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
        landing,
        friction,
        daily: day_series(&calendar, &days),
        daily_tokens: token_day_series(&calendar, &token_days),
        hour_of_day,
        by_workspace,
        by_runtime,
        by_source,
        by_account,
        agent_dispatched,
        tokens_by_model,
        tokens_by_runtime,
    }
}

pub fn run(paths: &Paths, _log: Arc<Logger>, since_days: Option<i64>) -> Result<Stats> {
    let db = Db::open(&paths.db())?;
    Ok(gather(&db.load_tasks()?, since_days))
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
            linear_team: None,
            linear_project: None,
            upstream: UpstreamState::Unstarted,
            local_done: false,
            pr_url: None,
            pr_number: None,
            pr_open: false,
            pr_merged: false,
            pr_mergeable: None,
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
    /// either one makes it an agent's, and an orchestrator only ever has the
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
            workspace: "offhand".into(),
            runtime: "claude-code".into(),
            account: None,
            worktree: None,
            branch: None,
            started_at: crate::db::rfc3339(start),
            ended_at: outcome
                .map(|_| crate::db::rfc3339(start + chrono::Duration::minutes(ran_for))),
            outcome,
            missing_ticks: 0,
            settled_at: None,
            reopened: 0,
            agent_status: None,
            dispatched_by: by.map(str::to_string),
            dispatched_by_pane: pane.map(str::to_string),
            base_sha: None,
            saw_working: true,
            screen_print: None,
            screen_at: None,
            nudges: 0,
            nudged_at: None,
            blocked_count: 0,
            overrun_warned_at: None,
            repo_path: Some("/repo/r".into()),
            collectable_at: None,
            collected_at: None,
            chat_archivable_at: None,
            chat_archived_at: None,
            dispatched_by_device: None,
            dispatched_by_user: None,
            billed_to: None,
            tokens: None,
            model: None,
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
    /// orchestrator has a pane and no task, and it still counts (AGE-24).
    #[test]
    fn an_orchestrators_releases_are_counted_too() {
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
        let s = gather(&tasks, Some(7));
        assert_eq!(s.daily.len(), 7);
        // Which local day each attempt lands in is not the point and must not
        // be assumed: at 00:05 the two are on opposite sides of midnight. The
        // properties are that the window has a bar per day, that the dispatches
        // are in the days they actually happened, and that every other day is
        // present and empty.
        let done_day = bucket_of(10);
        let cancelled_day = bucket_of(20);
        let bar = |date: &str| s.daily.iter().find(|d| d.date == date).unwrap_or_else(|| panic!("{date} is in the window"));
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
    }

    #[test]
    fn the_hour_histogram_has_a_slot_for_every_hour() {
        let tasks = vec![task("t1", vec![attempt(30, 5, Some(Outcome::Done), None)])];
        let s = gather(&tasks, None);
        assert_eq!(s.hour_of_day.len(), 24);
        assert_eq!(s.hour_of_day.iter().sum::<usize>(), 1);
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
        assert_eq!(s.landing.total(), s.tasks_touched);
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
        let spent = s.daily_tokens.iter().find(|d| d.date == day).expect("the day is in the window");
        assert_eq!(spent.usage.total(), 1_100);
        assert!(
            s.daily_tokens.iter().filter(|d| d.date != day).all(|d| d.usage.is_zero()),
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
        assert_eq!(gather(std::slice::from_ref(&t), None).tokens.total(), 10_010);
        assert_eq!(gather(&[t], Some(7)).tokens.total(), 110);
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
