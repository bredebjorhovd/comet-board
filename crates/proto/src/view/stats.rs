//! What the board knows about its own throughput, as every surface reads it.
//!
//! The numbers are gathered by `comet_board::stats` on the box, which owns
//! `board.db`; the *shape* lives here so a viewport can deserialize a
//! `BoardStats` reply without depending on the board crate — the same split
//! [`super::board::RuntimeOption`] makes, and for the same reason: the phone
//! and the laptop asking a remote box for its stats must not have to link a
//! SQLite store to read the answer.
//!
//! The derivations here are the ones a *renderer* needs and should not each
//! invent: ordering a tally, scaling a bar against the busiest bucket,
//! phrasing a duration. A stats page is mostly arithmetic on the way to a
//! layout, and arithmetic done twice is arithmetic done differently.
//!
//! Deliberately descriptive, like the CLI it shares a source with: it reports
//! what happened, it does not grade it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::TokenUsage;

/// One day's dispatches, for the throughput chart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DayBucket {
    /// `YYYY-MM-DD`, in the box's local reckoning — the day an operator means
    /// when they say "yesterday", not a UTC boundary that splits an evening.
    pub date: String,
    /// Attempts *started* in the day.
    pub dispatches: usize,
    /// Of those, how many have since ended `done`.
    pub done: usize,
}

/// One day's tokens, for the series drawn under the dispatch chart. Same days
/// and the same zero rule as [`DayBucket`] — the two series are generated from
/// one date range so a reader comparing them index by index is comparing the
/// same day.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenDay {
    /// `YYYY-MM-DD`, box-local, matching [`DayBucket::date`].
    pub date: String,
    /// Tokens on attempts *started* that day. A day whose attempts reported
    /// nothing is zero here and blank on the coverage line — the page says
    /// which of the two it is once, at the top, rather than per bar.
    pub usage: TokenUsage,
}

/// One row of a tally — a workspace, a runtime, a source, a person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tally {
    pub label: String,
    pub count: usize,
}

/// One row of a *token* tally — a model, a runtime. [`Tally`]'s shape with a
/// breakdown instead of a count, so the per-model table can show where the
/// tokens went and not only how many there were.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenTally {
    pub label: String,
    pub usage: TokenUsage,
}

/// Where the work ended up, which is the question a completion rate only
/// half-answers: an attempt can end `done` and leave a pull request nobody
/// merged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Landing {
    /// Tasks whose pull request merged.
    pub merged: usize,
    /// Tasks with a pull request still open.
    pub open: usize,
    /// Tasks whose pull request was closed without merging.
    pub closed_unmerged: usize,
    /// Tasks that were worked on and never opened one at all.
    pub no_pr: usize,
}

impl Landing {
    pub fn total(&self) -> usize {
        self.merged + self.open + self.closed_unmerged + self.no_pr
    }
}

/// The friction numbers — the board reporting on how often the work had to be
/// done twice, and on its own misjudgements.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Friction {
    /// Tasks that needed more than one attempt in the window.
    pub retried_tasks: usize,
    /// Times the board closed an attempt and then caught its agent still
    /// working. Not retries — nobody dispatched anything — so they are counted
    /// apart: this is the board reporting on its own judgement.
    pub early_settles: usize,
    /// Times an attempt *entered* blocked: the agent stopped to ask, or its run
    /// died. A transition count, not a tick count.
    pub blocked_entries: usize,
    /// Attempts that ran past their route's `max_duration` and were warned.
    pub overruns: usize,
}

impl Friction {
    /// Nothing to report is worth saying in one place rather than four.
    pub fn is_clean(&self) -> bool {
        self.retried_tasks == 0
            && self.early_settles == 0
            && self.blocked_entries == 0
            && self.overruns == 0
    }
}

/// Everything the board can say about its own throughput over a window.
///
/// Field order is the order a page reads them in: what happened, how well, how
/// long, where it landed, what it cost in friction, and who did it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardStats {
    /// The window these numbers cover, in days. `None` means everything.
    pub since_days: Option<i64>,
    pub attempts: usize,
    pub tasks_touched: usize,
    /// Ended attempts by outcome name (`done`, `cancelled`, `failed`).
    pub outcomes: BTreeMap<String, usize>,
    /// Still running right now.
    pub live: usize,
    /// Attempts that ended `done` as a share of ended attempts. `None` when
    /// nothing has ended — which is not the same as zero, and a page that
    /// rendered it as 0% would be lying about a board that has just started.
    pub completion_rate: Option<f64>,

    /// Minutes from dispatch to a finished attempt. Median, because one agent
    /// left running overnight would drag a mean anywhere.
    pub median_minutes: Option<i64>,
    /// The ninth decile — what a slow one costs, without the single worst.
    pub p90_minutes: Option<i64>,
    pub longest_minutes: Option<i64>,
    /// Agent-minutes in the window, summed over ended attempts.
    pub total_minutes: i64,

    /// Tokens across every attempt in the window that reported any (gh#151).
    /// Read it beside [`attempts_with_tokens`](Self::attempts_with_tokens):
    /// this is a total over the rows that answered, not over the window.
    pub tokens: TokenUsage,
    /// How many attempts in the window reported usage at all.
    ///
    /// The page's honesty line. Attempts that ran before the board recorded
    /// tokens — and any harness that never emits a usage event — are counted
    /// in [`attempts`](Self::attempts) and not here, so the totals above can
    /// be read as "of the work we can account for" instead of as the truth.
    pub attempts_with_tokens: usize,
    /// That share, `0.0..=1.0`. `None` when nothing ran, never `0.0` — the
    /// same rule [`completion_rate`](Self::completion_rate) follows, and for
    /// the same reason: a zero here would read as "nothing reported" when what
    /// happened is that nothing ran.
    pub token_coverage: Option<f64>,

    pub landing: Landing,
    pub friction: Friction,

    /// Attempts started per day, oldest first. Days with nothing are present
    /// with zeroes: a chart with holes in it reads as missing data.
    pub daily: Vec<DayBucket>,
    /// Tokens per day, index-aligned with [`daily`](Self::daily).
    pub daily_tokens: Vec<TokenDay>,
    /// Dispatches by hour of the box's local day, `[0..24)`.
    pub hour_of_day: Vec<usize>,

    pub by_workspace: BTreeMap<String, usize>,
    pub by_runtime: BTreeMap<String, usize>,
    pub by_source: BTreeMap<String, usize>,
    /// Whose subscription the attempts spent, by the login's own name.
    pub by_account: BTreeMap<String, usize>,
    /// Released by an agent rather than by a person.
    pub agent_dispatched: usize,

    /// Tokens by the model the harness said it was running (gh#151). Keyed on
    /// what the *run* reported, not on the route's override — most routes name
    /// no model, and a breakdown whose biggest row is "unknown" is not one.
    pub tokens_by_model: BTreeMap<String, TokenUsage>,
    /// Tokens by runtime — the provider split, against the same `by_runtime`
    /// dispatch counts already on this struct.
    pub tokens_by_runtime: BTreeMap<String, TokenUsage>,
}

impl BoardStats {
    /// An empty board — what a page shows before the first dispatch, and what
    /// a window with nothing in it honestly is.
    pub fn empty(since_days: Option<i64>) -> Self {
        Self {
            since_days,
            attempts: 0,
            tasks_touched: 0,
            outcomes: BTreeMap::new(),
            live: 0,
            completion_rate: None,
            median_minutes: None,
            p90_minutes: None,
            longest_minutes: None,
            total_minutes: 0,
            tokens: TokenUsage::default(),
            attempts_with_tokens: 0,
            token_coverage: None,
            landing: Landing::default(),
            friction: Friction::default(),
            daily: Vec::new(),
            daily_tokens: Vec::new(),
            hour_of_day: vec![0; 24],
            by_workspace: BTreeMap::new(),
            by_runtime: BTreeMap::new(),
            by_source: BTreeMap::new(),
            by_account: BTreeMap::new(),
            agent_dispatched: 0,
            tokens_by_model: BTreeMap::new(),
            tokens_by_runtime: BTreeMap::new(),
        }
    }

    /// Whether any attempt in the window reported tokens.
    ///
    /// The gate the token half of the page renders behind: with nothing
    /// reported there is no total to show, and a wall of zeroes would say the
    /// work was free rather than that it was never metered.
    pub fn has_tokens(&self) -> bool {
        self.attempts_with_tokens > 0
    }

    /// Nothing ran. Distinct from "nothing finished": the page says so instead
    /// of drawing a wall of zeroes and a 0% that means nothing.
    pub fn is_empty(&self) -> bool {
        self.attempts == 0
    }

    /// How the window is named on the page.
    pub fn window_label(&self) -> String {
        match self.since_days {
            Some(1) => "last 24 hours".to_string(),
            Some(d) => format!("last {d} days"),
            None => "all time".to_string(),
        }
    }
}

/// One tally, ordered for reading: biggest first, ties alphabetical so the rows
/// do not shuffle between refreshes of an unchanged board.
pub fn ranked(tally: &BTreeMap<String, usize>) -> Vec<Tally> {
    let mut rows: Vec<Tally> = tally
        .iter()
        .map(|(label, count)| Tally {
            label: label.clone(),
            count: *count,
        })
        .collect();
    rows.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.label.cmp(&b.label)));
    rows
}

/// The same, capped — with everything past the cap folded into one honest
/// `n others` row rather than dropped. A truncated list that does not say it
/// was truncated reads as the whole truth.
pub fn ranked_top(tally: &BTreeMap<String, usize>, max: usize) -> Vec<Tally> {
    let rows = ranked(tally);
    if max == 0 || rows.len() <= max {
        return rows;
    }
    let (head, tail) = rows.split_at(max);
    let rest: usize = tail.iter().map(|t| t.count).sum();
    let mut out = head.to_vec();
    out.push(Tally {
        label: format!("{} others", tail.len()),
        count: rest,
    });
    out
}

/// [`ranked_top`] for a token tally: biggest total first, ties alphabetical,
/// everything past the cap folded into one `n others` row that carries the
/// usage it stands for. Rows that spent nothing are dropped — a model that
/// appears with four zeroes is noise in a table about where tokens went.
pub fn ranked_tokens(tally: &BTreeMap<String, TokenUsage>, max: usize) -> Vec<TokenTally> {
    let mut rows: Vec<TokenTally> = tally
        .iter()
        .filter(|(_, usage)| !usage.is_zero())
        .map(|(label, usage)| TokenTally {
            label: label.clone(),
            usage: *usage,
        })
        .collect();
    rows.sort_by(|a, b| {
        b.usage
            .total()
            .cmp(&a.usage.total())
            .then_with(|| a.label.cmp(&b.label))
    });
    if max == 0 || rows.len() <= max {
        return rows;
    }
    let (head, tail) = rows.split_at(max);
    let rest: TokenUsage = tail.iter().map(|t| t.usage).sum();
    let mut out = head.to_vec();
    out.push(TokenTally {
        label: format!("{} others", tail.len()),
        usage: rest,
    });
    out
}

/// A token count as a page shows it: `812`, `48.2k`, `1.31M`.
///
/// Three significant figures and no more. The exact number is never the point
/// — nobody acts on the difference between 1,310,442 and 1,310,443 — and a
/// seven-digit figure in a tile is a number the eye has to count digits in.
pub fn human_tokens(tokens: u64) -> String {
    const M: u64 = 1_000_000;
    const K: u64 = 1_000;
    if tokens >= 1_000 * M {
        return format!("{:.2}B", tokens as f64 / (1_000 * M) as f64);
    }
    if tokens >= M {
        return format!("{:.2}M", tokens as f64 / M as f64);
    }
    if tokens >= 10 * K {
        return format!("{:.0}k", tokens as f64 / K as f64);
    }
    if tokens >= K {
        return format!("{:.1}k", tokens as f64 / K as f64);
    }
    tokens.to_string()
}

/// A bar's share of its chart, `0.0..=1.0`, scaled against the largest bucket.
///
/// Against the largest and not against the total: these charts answer "which
/// day was busy", and a proportion-of-total bar on a thirty-day window is
/// thirty bars all too short to compare.
pub fn bar_fraction(value: usize, peak: usize) -> f32 {
    if peak == 0 {
        return 0.0;
    }
    (value as f32 / peak as f32).clamp(0.0, 1.0)
}

/// The busiest bucket in a day series — the scale every bar is drawn against.
pub fn peak_dispatches(daily: &[DayBucket]) -> usize {
    daily.iter().map(|d| d.dispatches).max().unwrap_or(0)
}

/// The same, for the token series.
pub fn peak_tokens(daily: &[TokenDay]) -> u64 {
    daily.iter().map(|d| d.usage.total()).max().unwrap_or(0)
}

/// A duration in minutes, said the way a person would: `48m`, `3h 20m`, `2d 4h`.
///
/// Not `human_window`'s job (that phrases retention windows, and rounds to one
/// unit); a duration a reader is comparing against another duration keeps its
/// second unit.
pub fn human_minutes(minutes: i64) -> String {
    let m = minutes.max(0);
    if m < 60 {
        return format!("{m}m");
    }
    let hours = m / 60;
    let rem = m % 60;
    if hours < 24 {
        return if rem == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h {rem}m")
        };
    }
    let days = hours / 24;
    let rem_h = hours % 24;
    if rem_h == 0 {
        format!("{days}d")
    } else {
        format!("{days}d {rem_h}h")
    }
}

/// A rate as a whole-number percentage. `None` stays `None` all the way to the
/// renderer — see [`BoardStats::completion_rate`].
pub fn percent(rate: Option<f64>) -> Option<String> {
    rate.map(|r| format!("{:.0}%", (r * 100.0).clamp(0.0, 100.0)))
}

/// The windows a stats page offers, and what each is called.
pub const WINDOWS: &[(Option<i64>, &str)] = &[
    (Some(1), "24h"),
    (Some(7), "7 days"),
    (Some(30), "30 days"),
    (None, "All time"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn tally(pairs: &[(&str, usize)]) -> BTreeMap<String, usize> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
    }

    #[test]
    fn a_tally_reads_biggest_first_and_ties_do_not_shuffle() {
        let rows = ranked(&tally(&[("attn", 3), ("comet", 9), ("zed", 3)]));
        let labels: Vec<&str> = rows.iter().map(|t| t.label.as_str()).collect();
        // Ties alphabetical, so an unchanged board redraws identically.
        assert_eq!(labels, ["comet", "attn", "zed"]);
    }

    #[test]
    fn a_capped_tally_says_what_it_folded_away() {
        let rows = ranked_top(
            &tally(&[("a", 5), ("b", 4), ("c", 3), ("d", 2), ("e", 1)]),
            2,
        );
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2].label, "3 others");
        // Nothing is dropped: the fold carries the count it stands for.
        assert_eq!(rows[2].count, 6);
        assert_eq!(rows.iter().map(|t| t.count).sum::<usize>(), 15);
    }

    #[test]
    fn a_tally_shorter_than_the_cap_is_left_alone() {
        let rows = ranked_top(&tally(&[("a", 2), ("b", 1)]), 5);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|t| !t.label.ends_with("others")));
    }

    #[test]
    fn bars_scale_against_the_busiest_day_not_the_total() {
        let daily = vec![
            DayBucket {
                date: "2026-08-06".into(),
                dispatches: 2,
                done: 2,
            },
            DayBucket {
                date: "2026-08-07".into(),
                dispatches: 8,
                done: 5,
            },
        ];
        let peak = peak_dispatches(&daily);
        assert_eq!(peak, 8);
        assert_eq!(bar_fraction(8, peak), 1.0);
        assert_eq!(bar_fraction(2, peak), 0.25);
        // An empty chart is flat, not a division by zero.
        assert_eq!(bar_fraction(0, 0), 0.0);
    }

    #[test]
    fn durations_keep_the_unit_a_reader_is_comparing_in() {
        assert_eq!(human_minutes(0), "0m");
        assert_eq!(human_minutes(48), "48m");
        assert_eq!(human_minutes(60), "1h");
        assert_eq!(human_minutes(200), "3h 20m");
        assert_eq!(human_minutes(24 * 60), "1d");
        assert_eq!(human_minutes(52 * 60), "2d 4h");
        // A clock that ran backwards is a bug upstream, not a negative label.
        assert_eq!(human_minutes(-5), "0m");
    }

    #[test]
    fn a_board_that_has_never_finished_anything_has_no_rate_to_show() {
        let s = BoardStats::empty(Some(7));
        assert!(s.is_empty());
        assert_eq!(percent(s.completion_rate), None);
        assert_eq!(s.window_label(), "last 7 days");
        assert_eq!(BoardStats::empty(None).window_label(), "all time");
        assert_eq!(BoardStats::empty(Some(1)).window_label(), "last 24 hours");
        // 24 hour slots exist even before anything has run in them.
        assert_eq!(s.hour_of_day.len(), 24);
    }

    #[test]
    fn a_percentage_is_whole_and_bounded() {
        assert_eq!(percent(Some(0.914)), Some("91%".into()));
        assert_eq!(percent(Some(1.0)), Some("100%".into()));
        assert_eq!(percent(Some(0.0)), Some("0%".into()));
    }

    #[test]
    fn a_clean_window_is_one_fact_and_not_four_zeroes() {
        assert!(Friction::default().is_clean());
        assert!(
            !Friction {
                blocked_entries: 1,
                ..Default::default()
            }
            .is_clean()
        );
    }

    #[test]
    fn landing_totals_the_tasks_it_accounts_for() {
        let l = Landing {
            merged: 9,
            open: 2,
            closed_unmerged: 1,
            no_pr: 3,
        };
        assert_eq!(l.total(), 15);
    }

    // -- tokens (gh#151) -----------------------------------------------------

    fn usage(input: u64, output: u64, read: u64, write: u64) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: read,
            cache_creation_tokens: write,
        }
    }

    #[test]
    fn a_token_tally_ranks_on_the_total_and_folds_the_tail_without_losing_it() {
        let tally: BTreeMap<String, TokenUsage> = [
            ("sonnet", usage(10, 1, 0, 0)),
            ("opus", usage(1_000, 100, 5_000, 0)),
            ("haiku", usage(500, 50, 0, 0)),
            ("cursor-small", usage(4, 1, 0, 0)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let rows = ranked_tokens(&tally, 2);
        assert_eq!(rows[0].label, "opus");
        assert_eq!(rows[1].label, "haiku");
        assert_eq!(rows[2].label, "2 others");
        // Nothing dropped: the fold carries what it stands for.
        assert_eq!(rows[2].usage.total(), 16);
        assert_eq!(
            rows.iter().map(|t| t.usage.total()).sum::<u64>(),
            tally.values().map(|u| u.total()).sum::<u64>()
        );
    }

    #[test]
    fn a_model_that_spent_nothing_is_not_a_row_in_a_table_about_spending() {
        let tally: BTreeMap<String, TokenUsage> = [
            ("mock".to_string(), TokenUsage::default()),
            ("opus".to_string(), usage(1, 1, 0, 0)),
        ]
        .into_iter()
        .collect();
        let rows = ranked_tokens(&tally, 6);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "opus");
    }

    #[test]
    fn token_counts_are_readable_at_a_glance_not_exact() {
        assert_eq!(human_tokens(0), "0");
        assert_eq!(human_tokens(812), "812");
        assert_eq!(human_tokens(1_240), "1.2k");
        assert_eq!(human_tokens(48_200), "48k");
        assert_eq!(human_tokens(1_310_442), "1.31M");
        assert_eq!(human_tokens(2_500_000_000), "2.50B");
    }

    #[test]
    fn a_window_with_no_metered_attempt_shows_no_total_rather_than_a_free_one() {
        // The rule the rest of the page follows: a blank, never a zero. A 0
        // token total would read as work that cost nothing.
        let s = BoardStats::empty(Some(7));
        assert!(!s.has_tokens());
        assert_eq!(s.token_coverage, None);
        assert!(s.tokens.is_zero());
        // A window where attempts ran and none reported is a *real* 0% — that
        // one is worth saying out loud.
        let mut ran = BoardStats::empty(Some(7));
        ran.attempts = 4;
        ran.token_coverage = Some(0.0);
        assert!(!ran.has_tokens());
        assert_eq!(percent(ran.token_coverage), Some("0%".into()));
    }

    #[test]
    fn the_token_series_scales_against_the_busiest_day() {
        let daily = vec![
            TokenDay {
                date: "2026-08-07".into(),
                usage: usage(100, 10, 0, 0),
            },
            TokenDay {
                date: "2026-08-08".into(),
                usage: usage(400, 40, 0, 0),
            },
        ];
        assert_eq!(peak_tokens(&daily), 440);
        assert_eq!(peak_tokens(&[]), 0);
    }

    #[test]
    fn the_wire_shape_round_trips_camel_case() {
        // The board gathers this on the box and a viewport deserializes it
        // over the relay; a field whose casing disagrees would arrive as a
        // default and read as a real zero.
        let mut s = BoardStats::empty(Some(30));
        s.attempts = 4;
        s.tasks_touched = 3;
        s.completion_rate = Some(0.75);
        s.median_minutes = Some(18);
        s.p90_minutes = Some(90);
        s.total_minutes = 140;
        s.landing = Landing {
            merged: 2,
            open: 1,
            closed_unmerged: 0,
            no_pr: 0,
        };
        s.friction = Friction {
            retried_tasks: 1,
            early_settles: 0,
            blocked_entries: 2,
            overruns: 0,
        };
        s.daily = vec![DayBucket {
            date: "2026-08-08".into(),
            dispatches: 4,
            done: 3,
        }];
        s.by_workspace.insert("attn".into(), 4);
        s.tokens = usage(1_000, 200, 40_000, 3_000);
        s.attempts_with_tokens = 3;
        s.token_coverage = Some(0.75);
        s.daily_tokens = vec![TokenDay {
            date: "2026-08-08".into(),
            usage: usage(1_000, 200, 40_000, 3_000),
        }];
        s.tokens_by_model
            .insert("claude-opus-5".into(), usage(1_000, 200, 40_000, 3_000));
        s.tokens_by_runtime
            .insert("claude-code".into(), usage(1_000, 200, 40_000, 3_000));

        let json = serde_json::to_value(&s).expect("serializes");
        // The names the viewports read.
        assert!(json.get("sinceDays").is_some());
        assert!(json.get("medianMinutes").is_some());
        assert!(json.get("p90Minutes").is_some());
        assert!(json.get("totalMinutes").is_some());
        assert!(json.get("hourOfDay").is_some());
        assert!(json.get("byWorkspace").is_some());
        assert!(json.get("agentDispatched").is_some());
        assert!(json["landing"].get("closedUnmerged").is_some());
        assert!(json["landing"].get("noPr").is_some());
        assert!(json["friction"].get("retriedTasks").is_some());
        assert!(json["friction"].get("blockedEntries").is_some());
        assert!(json["daily"][0].get("dispatches").is_some());
        assert!(json.get("attemptsWithTokens").is_some());
        assert!(json.get("tokenCoverage").is_some());
        assert!(json.get("tokensByModel").is_some());
        assert!(json.get("tokensByRuntime").is_some());
        assert_eq!(json["tokens"]["cacheReadTokens"], 40_000);
        assert_eq!(json["dailyTokens"][0]["usage"]["cacheCreationTokens"], 3_000);

        let back: BoardStats = serde_json::from_value(json).expect("deserializes");
        assert_eq!(back, s);
    }
}
