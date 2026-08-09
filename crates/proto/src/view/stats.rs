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
use crate::view::rates::{ModelRate, RateSource, RateTable, Usd, human_usd};

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

// -- spend (gh#182) ----------------------------------------------------------

/// One model's tokens, priced.
///
/// Carries the rate it was priced at, not only the answer: a figure with no
/// provenance is one nobody can check, and the difference between an exact
/// rate and a family one ([`crate::view::rates::RateMatch::key`]) is the first
/// thing a reader who distrusts the number will want.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSpend {
    /// The model as the *run* reported it — the same label
    /// [`BoardStats::tokens_by_model`] is keyed on.
    pub label: String,
    /// The table key that priced it. Equal to `label` on an exact hit; the
    /// family it fell back to otherwise.
    pub rate_key: String,
    pub source: RateSource,
    pub rate: ModelRate,
    pub usage: TokenUsage,
    pub cost: Usd,
}

/// What one agent-account's work would have cost at the meter, beside what its
/// subscription actually costs.
///
/// The two halves are deliberately not added together, ever. The list price is
/// the board's; the plan is one person's. On a box carrying several teammates'
/// slots (gh#59) a single "cost" field would quietly sum other people's plans
/// into one number and call it the board's spend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSpend {
    /// Whose subscription — the same key [`BoardStats::by_account`] uses: the
    /// login's email, or the box's own login said out loud.
    pub label: String,
    pub attempts: usize,
    pub usage: TokenUsage,
    /// List price of this account's metered work. Unpriced models are excluded
    /// and counted in [`unpriced_tokens`](Self::unpriced_tokens).
    pub list_price: Usd,
    pub unpriced_tokens: u64,
    /// What the operator says this account's plan costs per month, from
    /// `[account."<id>"]` in `routing.toml`. `None` is unconfigured, which is
    /// not zero: comet cannot see anybody's bill and must not pretend to.
    pub plan: Option<AccountPlan>,
    /// The plan's cost over *this window*, pro-rated from the monthly figure —
    /// the only form in which the subsidy question has an answer. `None` for an
    /// all-time window (nothing to pro-rate against) or an unconfigured plan.
    pub plan_in_window: Option<Usd>,
}

impl AccountSpend {
    /// How far the subscription carried it: list price as a multiple of what
    /// the plan cost over the same window. `None` when either half is missing
    /// or the plan is free — a ratio against zero is not a number.
    pub fn subsidy(&self) -> Option<f64> {
        let plan = self.plan_in_window?;
        (!plan.is_zero()).then(|| self.list_price.dollars() / plan.dollars())
    }
}

/// A plan a human wrote down: what an agent account costs its owner per month.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountPlan {
    /// What the plan is called, as the operator wrote it (`Max 20x`,
    /// `Team seat`). Free-text: comet has no list of plans and inventing one
    /// would put words in an operator's mouth.
    pub label: Option<String>,
    pub monthly: Usd,
}

/// What the window cost — the half of the stats page that is about money
/// (gh#182).
///
/// Two different facts live here and are kept apart on purpose:
///
/// 1. **List price** ([`list_price`](Self::list_price)) — what the tokens the
///    board ran would have cost at the meter. That is the board's own number,
///    summed over every model it could price.
/// 2. **Subscriptions** ([`accounts`](Self::accounts)) — what the operator
///    actually pays for the plans those runs spent. Per account, entered by a
///    person, and never added into (1).
///
/// The comparison between them is the headline this exists for — *how
/// subsidised is this* rather than *what did I burn* — and it only works while
/// the two stay separate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardSpend {
    /// The rate set these figures were computed from, date and all.
    pub rates: RateTable,
    /// List price of everything the board could price in this window.
    pub list_price: Usd,
    /// Per model, biggest first. Ties alphabetical, like every other tally
    /// here, so an unchanged board redraws identically.
    pub by_model: Vec<ModelSpend>,
    /// Models with tokens and no rate. Present, and never folded into the
    /// total: a breakdown that dropped what it could not price would not add up
    /// to the token counts on the same page.
    pub unpriced: Vec<TokenTally>,
    /// Their tokens, summed — the one number that says how much of the window
    /// the price above does *not* account for.
    pub unpriced_tokens: u64,
    /// Per agent-account, biggest list price first.
    pub accounts: Vec<AccountSpend>,
}

impl BoardSpend {
    /// Did every metered model have a rate? The honesty gate for the headline,
    /// the way `token_coverage` is for the token totals.
    pub fn is_complete(&self) -> bool {
        self.unpriced.is_empty()
    }

    /// Any money to show at all. False on a window that ran nothing, and on one
    /// whose every model was unpriced.
    pub fn has_price(&self) -> bool {
        !self.by_model.is_empty()
    }

    /// The headline, said once: `$12.40 at list price`, with what it could not
    /// account for attached rather than left implied.
    pub fn headline(&self) -> String {
        let price = human_usd(self.list_price);
        if self.is_complete() {
            format!("{price} at list price")
        } else {
            format!(
                "{price} at list price, plus {} unpriced token(s) across {} model(s)",
                human_tokens(self.unpriced_tokens),
                self.unpriced.len()
            )
        }
    }

    /// What the operator pays per month across every account with a plan
    /// configured. Their plans, not the board's spend — see the type's own doc.
    pub fn monthly_subscriptions(&self) -> Usd {
        self.accounts
            .iter()
            .filter_map(|a| a.plan.as_ref().map(|p| p.monthly))
            .sum()
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
    /// Tokens by whose subscription paid for them (gh#182), keyed like
    /// [`by_account`](Self::by_account). The counts there say who ran how many
    /// attempts; this says what those attempts actually spent, which is what
    /// the per-account price is computed from.
    pub tokens_by_account: BTreeMap<String, TokenUsage>,

    /// What it cost, at list price, and what the plans behind it cost (gh#182).
    ///
    /// `None` is **rates not configured** — said out loud rather than rendered
    /// as a confident `$0.00`, which is gh#96's lesson applied to money. A
    /// board that was given rates and simply spent nothing carries a `Some`
    /// whose total is zero, and those two are different facts.
    #[serde(default)]
    pub spend: Option<BoardSpend>,
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
            tokens_by_account: BTreeMap::new(),
            spend: None,
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

    /// Is there a priced figure to show (gh#182)?
    ///
    /// False covers both halves of "no": a board with no rates configured, and
    /// a board with rates whose every metered model was one the table has never
    /// heard of. The page distinguishes them with [`spend`](Self::spend) being
    /// `None` versus a `Some` that
    /// [`has_price`](BoardSpend::has_price)s nothing.
    pub fn has_spend(&self) -> bool {
        self.spend.as_ref().is_some_and(BoardSpend::has_price)
    }

    /// The sentence a page leads the money half with — including the two ways
    /// there is no number, which are the ones worth saying out loud.
    pub fn spend_label(&self) -> String {
        match &self.spend {
            None => "rates not configured".to_string(),
            Some(spend) if !spend.has_price() && spend.unpriced_tokens > 0 => format!(
                "no rate for any model in this window ({} unpriced token(s))",
                human_tokens(spend.unpriced_tokens)
            ),
            Some(spend) if !spend.has_price() => "nothing metered to price".to_string(),
            Some(spend) => spend.headline(),
        }
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

/// The cross-language fixture (gh#157).
///
/// The rules above have a second implementation: `apps/ios/Comet/Board/
/// StatsModels.swift`, because no Rust runs on that device. Two
/// implementations of one rule is exactly what this module's own doc calls a
/// real bug waiting to happen — the phone quietly disagreeing with the desktop
/// about a number somebody is deciding on — and the tests above only ever
/// guarded the half written in Rust.
///
/// So the *cases* leave Rust as data. This module writes every rule's inputs
/// and expected outputs to `apps/ios/Comet/Spec/stats-spec.json`, asserts the
/// checked-in file still matches what the code here produces, and the phone's
/// `SpecRunner` asserts its own functions against the same file. One spec, two
/// consumers: whichever side moves is the side that fails.
///
/// Regenerate after changing a rule:
///
/// ```sh
/// UPDATE_STATS_SPEC=1 cargo test -p comet-proto stats
/// ```
///
/// **Then run `scripts/ios-stats-spec.sh`.** This half runs in CI and the
/// Swift half does not — it needs a simulator — so regenerating alone turns
/// the build green while leaving the phone wrong about the rule that just
/// changed. The guard below is a prompt, not an enforcement: what it tells you
/// is that a second implementation exists and now disagrees.
#[cfg(test)]
mod spec {
    use super::*;
    use serde_json::{Value, json};

    fn tally(pairs: &[(&str, usize)]) -> BTreeMap<String, usize> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
    }

    fn usage(input: u64, output: u64, read: u64, write: u64) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: read,
            cache_creation_tokens: write,
        }
    }

    fn token_tally(pairs: &[(&str, TokenUsage)]) -> BTreeMap<String, TokenUsage> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
    }

    fn day(date: &str, dispatches: usize, done: usize) -> DayBucket {
        DayBucket {
            date: date.to_string(),
            dispatches,
            done,
        }
    }

    fn spec_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../apps/ios/Comet/Spec/stats-spec.json")
    }

    /// One `ranked_top` case: a tally, a cap, and the rows it must produce.
    fn ranked_case(name: &str, pairs: &[(&str, usize)], max: usize) -> Value {
        let input = tally(pairs);
        json!({ "name": name, "tally": input, "max": max, "expect": ranked_top(&input, max) })
    }

    fn token_case(name: &str, pairs: &[(&str, TokenUsage)], max: usize) -> Value {
        let input = token_tally(pairs);
        json!({ "name": name, "tally": input, "max": max, "expect": ranked_tokens(&input, max) })
    }

    /// One whole-struct case: a serialized `BoardStats` and every answer the
    /// struct's own accessors give about it. Doubles as a decode test — the
    /// phone has to read this shape off the wire before it can render it.
    fn stats_case(name: &str, stats: &BoardStats) -> Value {
        json!({
            "name": name,
            "stats": stats,
            "expect": {
                "windowLabel": stats.window_label(),
                "isEmpty": stats.is_empty(),
                "hasTokens": stats.has_tokens(),
                "completionPercent": percent(stats.completion_rate),
                "coveragePercent": percent(stats.token_coverage),
                "tokenTotal": stats.tokens.total(),
                "peakDispatches": peak_dispatches(&stats.daily),
                "peakTokens": peak_tokens(&stats.daily_tokens),
                // gh#182. The three states are the whole point: no rates, rates
                // that priced nothing, and a real figure — and a surface that
                // collapsed any two of them would be inventing a zero.
                "hasSpend": stats.has_spend(),
                "spendLabel": stats.spend_label(),
            }
        })
    }

    /// A priced window, built by hand (gh#182).
    ///
    /// The arithmetic that produces one of these lives in `comet_board::prices`
    /// — the box's side, where the config is — and is tested there. What the
    /// fixture pins is what both viewports do *with* it: the sentences, and the
    /// decode.
    fn spend(
        rates: RateTable,
        models: &[(&str, TokenUsage)],
        accounts: &[AccountSpend],
    ) -> BoardSpend {
        let mut by_model = Vec::new();
        let mut unpriced = Vec::new();
        for (label, usage) in models {
            match rates.rate_for(label) {
                Some(found) => by_model.push(ModelSpend {
                    label: (*label).to_string(),
                    rate_key: found.key,
                    source: found.source,
                    rate: found.rate,
                    usage: *usage,
                    cost: found.rate.cost(*usage),
                }),
                None => unpriced.push(TokenTally {
                    label: (*label).to_string(),
                    usage: *usage,
                }),
            }
        }
        BoardSpend {
            rates,
            list_price: by_model.iter().map(|m| m.cost).sum(),
            unpriced_tokens: unpriced.iter().map(|t| t.usage.total()).sum(),
            by_model,
            unpriced,
            accounts: accounts.to_vec(),
        }
    }

    /// Every rule this module owns, as data.
    fn build() -> Value {
        // A board mid-window: things ended, things merged, tokens partly
        // metered — the case where every accessor has something to say.
        let mut busy = BoardStats::empty(Some(7));
        busy.attempts = 13;
        busy.tasks_touched = 11;
        busy.live = 2;
        busy.completion_rate = Some(0.8181818181818182);
        busy.median_minutes = Some(18);
        busy.p90_minutes = Some(74);
        busy.total_minutes = 351;
        busy.tokens = usage(9_400, 6_100, 148_000, 21_500);
        busy.attempts_with_tokens = 8;
        busy.token_coverage = Some(8.0 / 13.0);
        busy.landing = Landing {
            merged: 7,
            open: 2,
            closed_unmerged: 1,
            no_pr: 1,
        };
        busy.friction = Friction {
            retried_tasks: 3,
            early_settles: 1,
            blocked_entries: 4,
            overruns: 1,
        };
        busy.daily = vec![
            day("2026-08-03", 2, 2),
            day("2026-08-04", 0, 0),
            day("2026-08-05", 8, 5),
            day("2026-08-06", 3, 3),
        ];
        busy.daily_tokens = vec![
            TokenDay {
                date: "2026-08-03".into(),
                usage: usage(100, 10, 0, 0),
            },
            TokenDay {
                date: "2026-08-04".into(),
                usage: TokenUsage::default(),
            },
            TokenDay {
                date: "2026-08-05".into(),
                usage: usage(400, 40, 0, 0),
            },
            TokenDay {
                date: "2026-08-06".into(),
                usage: usage(50, 5, 0, 0),
            },
        ];
        busy.by_workspace = tally(&[("comet-native", 9), ("edge", 4)]);
        busy.by_runtime = tally(&[("claude-code", 10), ("codex", 3)]);
        busy.agent_dispatched = 5;
        busy.tokens_by_model = token_tally(&[
            ("claude-opus-5", usage(9_000, 6_000, 148_000, 21_000)),
            ("gpt-5.6-terra", usage(400, 100, 0, 500)),
        ]);

        // Attempts ran and none reported usage: a REAL 0% coverage, which is a
        // different fact from "nothing ran" and the one most easily lost.
        let mut unmetered = BoardStats::empty(Some(30));
        unmetered.attempts = 4;
        unmetered.token_coverage = Some(0.0);

        // The same busy week, priced (gh#182): one model the table knows, one
        // it does not, and one account whose plan the operator wrote down. The
        // three facts a spend page must not collapse — a total, what that total
        // leaves out, and what the subscription behind it costs.
        let mut priced = busy.clone();
        priced.by_account = tally(&[("brede@tally.no", 11), ("ana@example.com", 2)]);
        priced.tokens_by_account = token_tally(&[
            ("brede@tally.no", usage(9_000, 6_000, 148_000, 21_000)),
            ("ana@example.com", usage(400, 100, 0, 500)),
        ]);
        priced.spend = Some(spend(
            crate::view::rates::builtin(),
            &[
                ("claude-opus-5", usage(9_000, 6_000, 148_000, 21_000)),
                ("gpt-5.6-terra", usage(400, 100, 0, 500)),
            ],
            &[
                AccountSpend {
                    label: "brede@tally.no".into(),
                    attempts: 11,
                    usage: usage(9_000, 6_000, 148_000, 21_000),
                    // $0.045 fresh input + $0.15 output + $0.074 cache reads +
                    // $0.13125 cache writes — the four rates, applied apart.
                    list_price: Usd::from_dollars(0.400_25),
                    unpriced_tokens: 0,
                    plan: Some(AccountPlan {
                        label: Some("Claude Max 20x".into()),
                        monthly: Usd::from_dollars(200.0),
                    }),
                    plan_in_window: Some(Usd::from_dollars(46.666_667)),
                },
                AccountSpend {
                    label: "ana@example.com".into(),
                    attempts: 2,
                    usage: usage(400, 100, 0, 500),
                    list_price: Usd::ZERO,
                    unpriced_tokens: 1_000,
                    plan: None,
                    plan_in_window: None,
                },
            ],
        ));

        // Rates configured, and not one of them matched: a real answer, and not
        // the same one as "no rates configured" above.
        let mut nothing_priceable = BoardStats::empty(Some(7));
        nothing_priceable.attempts = 2;
        nothing_priceable.attempts_with_tokens = 2;
        nothing_priceable.token_coverage = Some(1.0);
        nothing_priceable.tokens = usage(400, 100, 0, 500);
        nothing_priceable.tokens_by_model =
            token_tally(&[("gpt-5.6-terra", usage(400, 100, 0, 500))]);
        nothing_priceable.spend = Some(spend(
            crate::view::rates::builtin(),
            &[("gpt-5.6-terra", usage(400, 100, 0, 500))],
            &[],
        ));

        // Scalar rules, one case per input. Built before the object below
        // because `json!` reads a `[` as an array literal, not as a Rust one.
        let human_token_cases: Vec<Value> = [
            0_u64,
            812,
            1_240,
            9_999,
            10_000,
            48_200,
            1_310_442,
            2_500_000_000,
        ]
        .iter()
        .map(|t| json!({ "tokens": t, "expect": human_tokens(*t) }))
        .collect();
        let human_minute_cases: Vec<Value> = [-5_i64, 0, 48, 59, 60, 200, 1_440, 1_500, 3_120]
            .iter()
            .map(|m| json!({ "minutes": m, "expect": human_minutes(*m) }))
            .collect();
        // None stays None all the way to the renderer: a rate that has not
        // happened yet is not 0%.
        let percent_cases: Vec<Value> = [
            None,
            Some(0.0),
            Some(0.914),
            Some(1.0),
            Some(1.4),
            Some(-0.2),
        ]
        .iter()
        .map(|r| json!({ "rate": r, "expect": percent(*r) }))
        .collect();
        // Against the largest bucket, never the total.
        let bar_cases: Vec<Value> = [(8_usize, 8_usize), (2, 8), (0, 8), (0, 0), (9, 8)]
            .iter()
            .map(|(v, p)| json!({ "value": v, "peak": p, "expect": bar_fraction(*v, *p) }))
            .collect();
        // Money, at the three scales the same field is read at (gh#182): a
        // per-model row where the cents are the whole story, a headline, and a
        // figure nobody acts on to the cent. Halves are deliberately absent —
        // two languages' rounding of `1.315` is not a rule worth pinning.
        let usd_cases: Vec<Value> = [0.0_f64, 0.0042, 0.14, 1.316, 12.4, 248.4, 1_234.4]
            .iter()
            .map(|d| {
                let amount = crate::view::rates::Usd::from_dollars(*d);
                json!({ "dollars": amount, "expect": crate::view::rates::human_usd(amount) })
            })
            .collect();

        json!({
            "note": "Generated by crates/proto/src/view/stats.rs (mod spec). \
                     Do not hand-edit — run `UPDATE_STATS_SPEC=1 cargo test -p comet-proto stats`.",
            "rankedTop": [
                // Ties alphabetical, so an unchanged board redraws identically.
                ranked_case("ties do not shuffle", &[("attn", 3), ("comet", 9), ("zed", 3)], 0),
                // The fold carries the count it stands for.
                ranked_case(
                    "the tail is folded, never dropped",
                    &[("a", 5), ("b", 4), ("c", 3), ("d", 2), ("e", 1)],
                    2,
                ),
                ranked_case("shorter than the cap is left alone", &[("a", 2), ("b", 1)], 5),
                ranked_case("nothing at all", &[], 4),
            ],
            "rankedTokens": [
                token_case(
                    "ranks on the total and folds the tail with its usage",
                    &[
                        ("sonnet", usage(10, 1, 0, 0)),
                        ("opus", usage(1_000, 100, 5_000, 0)),
                        ("haiku", usage(500, 50, 0, 0)),
                        ("cursor-small", usage(4, 1, 0, 0)),
                    ],
                    2,
                ),
                token_case(
                    "a model that spent nothing is not a row",
                    &[("mock", TokenUsage::default()), ("opus", usage(1, 1, 0, 0))],
                    6,
                ),
            ],
            "humanTokens": human_token_cases,
            "humanMinutes": human_minute_cases,
            "percent": percent_cases,
            "barFraction": bar_cases,
            "humanUsd": usd_cases,
            "boardStats": [
                stats_case("a board that has just started", &BoardStats::empty(Some(7))),
                stats_case("all time, nothing in it", &BoardStats::empty(None)),
                stats_case("24 hours", &BoardStats::empty(Some(1))),
                stats_case("ran, and nothing reported usage", &unmetered),
                stats_case("a busy week", &busy),
                stats_case("a busy week, priced", &priced),
                stats_case("rates configured, nothing priceable", &nothing_priceable),
            ],
        })
    }

    #[test]
    fn the_cross_language_fixture_matches_this_module() {
        let built = build();
        let rendered = format!(
            "{}\n",
            serde_json::to_string_pretty(&built).expect("serializes")
        );
        let path = spec_path();
        if std::env::var("UPDATE_STATS_SPEC").is_ok() {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).expect("fixture directory");
            }
            std::fs::write(&path, &rendered).expect("writes the fixture");
            return;
        }
        let on_disk = std::fs::read_to_string(&path).unwrap_or_else(|err| {
            panic!(
                "cannot read {}: {err}. Run `UPDATE_STATS_SPEC=1 cargo test -p comet-proto stats`",
                path.display()
            )
        });
        // Compare as JSON, not as text: reformatting the file is not a
        // behaviour change, and a rule moving is.
        let disk_value: Value = serde_json::from_str(&on_disk).expect("the fixture is JSON");
        if disk_value == built {
            return;
        }
        // Report the sections that moved, not the whole document. A diff of
        // eight hundred lines is a diff nobody reads, and the useful fact here
        // is *which rule* changed.
        let (disk_map, built_map) = (
            disk_value.as_object().expect("an object"),
            built.as_object().expect("an object"),
        );
        let moved: Vec<String> = built_map
            .keys()
            .chain(disk_map.keys())
            .filter(|key| disk_map.get(*key) != built_map.get(*key))
            .map(|key| {
                format!(
                    "  {key}:\n    fixture: {}\n    now:     {}",
                    disk_map
                        .get(key)
                        .map(ToString::to_string)
                        .unwrap_or_else(|| "(absent)".into()),
                    built_map
                        .get(key)
                        .map(ToString::to_string)
                        .unwrap_or_else(|| "(absent)".into()),
                )
            })
            .collect();
        panic!(
            "the checked-in stats fixture no longer matches this module — a rule changed here, \
             so the phone's copy in apps/ios/Comet/Board/StatsModels.swift is now wrong too.\n\n\
             {}\n\n\
             Regenerate with `UPDATE_STATS_SPEC=1 cargo test -p comet-proto stats` — and then \
             run `scripts/ios-stats-spec.sh` and fix what it reports. Regenerating alone makes \
             THIS test pass and leaves the phone wrong: the Swift half needs a simulator and \
             does not run in CI, so nothing else will tell you.",
            moved.join("\n")
        );
    }
}
