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

/// One day's tokens, which is what the day chart is drawn from (gh#226). Same
/// days and the same zero rule as [`DayBucket`] — the two series are generated
/// from one date range so a reader comparing them index by index is comparing
/// the same day.
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

/// One column of the day chart: how tall it is, and the two lines around it
/// (gh#226).
///
/// **Height is tokens, and the dispatch count rides in the caption.** The chart
/// used to plot dispatches, which is nearly flat and nearly meaningless — two
/// dispatches can differ by twenty times in what they cost. Token volume is
/// what actually varies between days, and it is what the spend block above is
/// computed from, so the two blocks now tell one story instead of two.
///
/// Nothing here is a new number: [`DayBucket`] and [`TokenDay`] are the same
/// two series the board already gathered, zipped by date so a spike cannot land
/// under the wrong day even if a board answered with series of different
/// lengths.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DayColumn {
    /// `YYYY-MM-DD`, box-local, from [`DayBucket::date`].
    pub date: String,
    /// What the day spent — the bar's height.
    pub tokens: u64,
    /// What ran that day — the number in the caption.
    pub dispatches: usize,
    /// The bar's share of the chart, `0.0..=1.0`, against the busiest day.
    pub fraction: f32,
    /// What rides above the bar: `1.31M`, or `—` on a day with nothing to show.
    pub value: String,
    /// And under it: `Mon 3 · 2`.
    pub caption: String,
}

impl DayColumn {
    /// A day with no tokens on it — drawn as a hairline where the bar would be,
    /// never as an absent column. A seven-day window on a board that worked one
    /// day is a shape; one lonely bar reads as six days the board forgot.
    ///
    /// It does not distinguish "nothing ran" from "what ran reported nothing":
    /// the caption's dispatch count is the first of those, and the coverage line
    /// at the top of the block is the second, said once rather than per bar.
    pub fn is_quiet(&self) -> bool {
        self.tokens == 0
    }
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

/// Hours in the box's local day — the width of every hour series here.
pub const HOURS: usize = 24;

/// One row of the crossed grid (gh#179): a workspace, and its dispatches by
/// hour of the local day.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HourRow {
    pub label: String,
    /// [`HOURS`] slots, always all of them.
    pub hours: Vec<usize>,
    pub total: usize,
}

/// *When* the work is released crossed with *where* it goes (gh#179).
///
/// These were two cards — an hour histogram and a workspace tally — and the
/// fact worth having was in neither of them: that the evening releases all go
/// to one space, or that a space is only ever touched inside working hours. A
/// reader cannot recover a crossing from two margins, so the page draws the
/// crossing and keeps the margins on its edges.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HourGrid {
    /// Busiest space first, ties alphabetical, tail folded into `n others`.
    pub rows: Vec<HourRow>,
    /// The bottom margin: dispatches per hour across every row, folded ones
    /// included — the old hour histogram, in its place.
    pub columns: Vec<usize>,
    /// The busiest single cell, which is what the heat is scaled against.
    pub peak: usize,
    pub total: usize,
}

impl HourGrid {
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Where the work ended up, which is the question a completion rate only
/// half-answers: an attempt can end `done` and leave a pull request nobody
/// merged.
///
/// Four places, and the two at the bottom are the reason this is a shape and
/// not a merge count (gh#228). *Closed unmerged* is a pull request somebody
/// rejected or abandoned; *no PR raised* is an agent that settled having
/// produced nothing. They are the only numbers on a stats page that say the
/// board wasted its time, and a surface that folds either of them into "in
/// review" has hidden the losses behind the one word that reads as patience.
///
/// [`in_flight`](Self::in_flight) is deliberately outside the four: work still
/// running has not landed anywhere, and counting it under *no PR raised* —
/// which is what a rule keyed on PR presence alone does — reports an agent
/// that is still typing as an agent that came back empty.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Landing {
    /// Tasks whose pull request merged.
    pub merged: usize,
    /// Tasks with a pull request still open.
    pub open: usize,
    /// Tasks whose pull request was closed without merging.
    pub closed_unmerged: usize,
    /// Tasks whose attempts all settled and left no pull request behind.
    pub no_pr: usize,
    /// Tasks with no pull request and an attempt still going — not a landing,
    /// so it is not one of the four and never sits in the bar. A caption, at
    /// most.
    ///
    /// Defaulted on the wire: a board that predates the split (gh#228) answers
    /// without the key, and its four categories still decode.
    #[serde(default)]
    pub in_flight: usize,
}

/// One of the four places work lands, as an identity rather than a label.
///
/// Both viewports paint these from their own status ramp — merged is the
/// settled hue, an open pull request is the review hue, closed-unmerged is
/// blocked, nothing-raised is the working amber — and neither invents the
/// ordering or the wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LandingKind {
    Merged,
    Open,
    ClosedUnmerged,
    NoPr,
}

impl LandingKind {
    /// Best outcome first, so a bar reads left to right from landed to lost.
    pub const ALL: [LandingKind; 4] = [
        LandingKind::Merged,
        LandingKind::Open,
        LandingKind::ClosedUnmerged,
        LandingKind::NoPr,
    ];

    /// What a legend calls it. `PR open` rather than `In review`: review is a
    /// state a human is in, and the fact here is that the branch exists and
    /// has not been taken.
    pub fn label(self) -> &'static str {
        match self {
            LandingKind::Merged => "Merged",
            LandingKind::Open => "PR open",
            LandingKind::ClosedUnmerged => "Closed unmerged",
            LandingKind::NoPr => "No PR raised",
        }
    }
}

/// One band of the landing bar: what it is, how many tasks, and the share of
/// the bar it takes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LandingSegment {
    pub kind: LandingKind,
    pub label: String,
    pub count: usize,
    /// Share of [`Landing::total`], `0.0..=1.0`. Zero when the window landed
    /// nothing — a bar over no tasks is not drawn, it is not drawn empty.
    pub fraction: f64,
}

impl Landing {
    /// The four that landed. Excludes [`in_flight`](Self::in_flight): a bar
    /// whose widths were shares of unfinished work would move under the reader
    /// as attempts end without a single task landing anywhere.
    pub fn total(&self) -> usize {
        self.merged + self.open + self.closed_unmerged + self.no_pr
    }

    /// Every task this accounts for, landed or still going — what the window
    /// touched.
    pub fn touched(&self) -> usize {
        self.total() + self.in_flight
    }

    pub fn count(&self, kind: LandingKind) -> usize {
        match kind {
            LandingKind::Merged => self.merged,
            LandingKind::Open => self.open,
            LandingKind::ClosedUnmerged => self.closed_unmerged,
            LandingKind::NoPr => self.no_pr,
        }
    }

    /// The bar, as bands. **All four, always** — including the empty ones,
    /// which are the point: a legend that drops `Closed unmerged 0` is a
    /// legend where a reader cannot tell a window that lost nothing from a
    /// surface that does not count losses. The renderer draws the non-empty
    /// ones and lists them all.
    pub fn segments(&self) -> Vec<LandingSegment> {
        let total = self.total();
        LandingKind::ALL
            .iter()
            .map(|kind| {
                let count = self.count(*kind);
                LandingSegment {
                    kind: *kind,
                    label: kind.label().to_string(),
                    count,
                    fraction: if total == 0 {
                        0.0
                    } else {
                        count as f64 / total as f64
                    },
                }
            })
            .collect()
    }

    /// `11 tasks` — the headline over the bar. Tasks, never attempts: three
    /// goes at one issue produce one pull request, and counting the attempts
    /// would report the same merge three times.
    pub fn headline(&self) -> String {
        let n = self.total();
        format!("{n} task{}", if n == 1 { "" } else { "s" })
    }

    /// The caption under it: what the bar leaves out. `None` when nothing is
    /// still running, because "0 still running" is a line that says nothing.
    pub fn in_flight_note(&self) -> Option<String> {
        (self.in_flight > 0)
            .then(|| format!("{} still running — not landed anywhere yet", self.in_flight))
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

    /// The same, pro-rated onto *this window* — the only form in which the two
    /// halves are comparable at all, and the denominator of
    /// [`subsidy`](Self::subsidy).
    ///
    /// `None` when not one account has a plan to pro-rate, which is a state the
    /// page says out loud: a missing plan is unentered, never free. Summing
    /// plans across accounts is the same arithmetic
    /// [`monthly_subscriptions`](Self::monthly_subscriptions) does, and just as
    /// carefully NOT added to the list price — the pair is a ratio, never a
    /// total.
    pub fn subscriptions_in_window(&self) -> Option<Usd> {
        let mut found = false;
        let mut total = Usd::ZERO;
        for account in &self.accounts {
            if let Some(share) = account.plan_in_window {
                found = true;
                total += share;
            }
        }
        found.then_some(total)
    }

    /// How far the subscriptions carried the window: list price as a multiple
    /// of what the plans behind it cost over the same days. The board-wide
    /// answer to [`AccountSpend::subsidy`]'s per-account one, and the headline
    /// the spend block exists for.
    ///
    /// `None` when there is nothing to divide by — no plan entered, or plans
    /// that are free. A ratio against zero is not a number.
    pub fn subsidy(&self) -> Option<f64> {
        let plan = self.subscriptions_in_window()?;
        (!plan.is_zero()).then(|| self.list_price.dollars() / plan.dollars())
    }

    /// Where the list price goes (gh#225): the same money, grouped by the kind
    /// of token it bought rather than by the model that bought it.
    ///
    /// The per-model table answers *which model was expensive*; this answers
    /// *which of the four rates was*, and they are not the same question. A week
    /// that spent most of its price on 1.2M output tokens and a tenth of it on
    /// 35M cached ones is a week where the cache is working and the writing is
    /// what costs — which nothing else on the page can tell you.
    ///
    /// Priced models only, exactly like [`list_price`](Self::list_price): a
    /// model the table has never heard of has no rate to attribute its tokens
    /// to, and guessing one would put a number under a bar that means nothing.
    pub fn cost_split(&self) -> CostSplit {
        let mut slices: Vec<CostSlice> = CostClass::ALL
            .into_iter()
            .map(|class| {
                let mut cost = Usd::ZERO;
                let mut tokens = 0u64;
                for model in &self.by_model {
                    let spent = class.tokens(model.usage);
                    tokens = tokens.saturating_add(spent);
                    cost += class.rate(&model.rate).per_million(spent);
                }
                CostSlice {
                    class,
                    cost,
                    tokens,
                    share: 0.0,
                }
            })
            // A class nobody spent anything in is not a segment of a bar about
            // where the money went — `ranked_tokens`' rule, one axis over.
            .filter(|slice| slice.tokens > 0 || !slice.cost.is_zero())
            .collect();
        slices.sort_by(|a, b| b.cost.cmp(&a.cost).then_with(|| a.class.cmp(&b.class)));

        let total: Usd = slices.iter().map(|s| s.cost).sum();
        let tokens: u64 = slices
            .iter()
            .fold(0u64, |sum, s| sum.saturating_add(s.tokens));
        if !total.is_zero() {
            for slice in &mut slices {
                slice.share = (slice.cost.dollars() / total.dollars()).clamp(0.0, 1.0);
            }
        }
        CostSplit {
            slices,
            total,
            tokens,
        }
    }
}

// -- where the list price goes (gh#225) --------------------------------------

/// One of the four ways a token costs money.
///
/// The rates are priced apart ([`ModelRate`], gh#182) and then every surface
/// adds them straight back up, so the page could say what a window cost and not
/// where the money went. That is the fact worth having: a coding agent's usage
/// is lopsided by construction — tens of millions of cached-input tokens
/// against a few hundred thousand output ones — and once priced, the small
/// bucket is usually the expensive one. The two readings point at opposite
/// fixes, and a single total points at neither.
///
/// Declared in order of what a token costs in each: output is the dearest per
/// token, a cache read a tenth of fresh input. That order is only the tie-break
/// — [`BoardSpend::cost_split`] ranks on money actually spent, which is a
/// different question and the one a reader is asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CostClass {
    Output,
    CacheWrite,
    Input,
    CacheRead,
}

impl CostClass {
    /// Every class, dearest per token first.
    pub const ALL: [CostClass; 4] = [
        CostClass::Output,
        CostClass::CacheWrite,
        CostClass::Input,
        CostClass::CacheRead,
    ];

    /// What a legend calls it. Said from the reader's side — `cached input`
    /// rather than `cache_read_tokens` — because the point of the split is that
    /// somebody who has never read [`TokenUsage`] can act on it.
    pub fn label(self) -> &'static str {
        match self {
            CostClass::Output => "output",
            CostClass::CacheWrite => "cache writes",
            CostClass::Input => "uncached input",
            CostClass::CacheRead => "cached input",
        }
    }

    /// The bucket of a usage this class prices.
    pub fn tokens(self, usage: TokenUsage) -> u64 {
        match self {
            CostClass::Output => usage.output_tokens,
            CostClass::CacheWrite => usage.cache_creation_tokens,
            CostClass::Input => usage.input_tokens,
            CostClass::CacheRead => usage.cache_read_tokens,
        }
    }

    /// The per-million rate that bucket is priced at.
    pub fn rate(self, rate: &ModelRate) -> Usd {
        match self {
            CostClass::Output => rate.output,
            CostClass::CacheWrite => rate.cache_write,
            CostClass::Input => rate.input,
            CostClass::CacheRead => rate.cache_read,
        }
    }
}

/// One class's share of the list price: what it cost, and what it cost it on.
///
/// Both halves, always. `$52` alone is a number to nod at; `$52 across 34.8M
/// tokens` beside `$93 across 1.24M` is the sentence the block exists to say.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostSlice {
    pub class: CostClass,
    pub cost: Usd,
    pub tokens: u64,
    /// Of the priced total, `0.0..=1.0` — the width of this segment of the bar.
    pub share: f64,
}

impl CostSlice {
    pub fn label(&self) -> &'static str {
        self.class.label()
    }

    /// The legend line: `output $93 / 1.24M`.
    pub fn legend(&self) -> String {
        format!(
            "{} {} / {}",
            self.class.label(),
            human_usd(self.cost),
            human_tokens(self.tokens)
        )
    }
}

/// The list price regrouped by what kind of token it was spent on (gh#225).
///
/// Nothing new is recorded and nothing is re-priced: this is the same four
/// products [`ModelRate::cost`] already sums per model, kept apart across
/// models instead of collapsed. Which is why [`total`](Self::total) is
/// [`BoardSpend::list_price`] exactly rather than nearly — the terms are the
/// same terms, rounded in the same places.
///
/// Unlike the rules beside it this one has no Swift counterpart yet: the
/// phone's stats screen draws no split, so there is nothing there to disagree
/// with it and no case in the cross-language fixture. When it does, this is the
/// arithmetic it adopts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostSplit {
    /// Biggest spend first, ties in [`CostClass::ALL`] order so an unchanged
    /// board redraws identically. A class with nothing in it is absent, not a
    /// zero-width segment nobody can hover.
    pub slices: Vec<CostSlice>,
    /// The four classes summed — the priced total, and the denominator every
    /// share is taken against.
    pub total: Usd,
    /// The tokens behind that total. Priced tokens only: what the board could
    /// not price is [`BoardSpend::unpriced_tokens`] and stays there.
    pub tokens: u64,
}

impl CostSplit {
    /// Nothing to draw. A window with no priced tokens, or one whose whole
    /// price rounds to zero — either way a bar of four empty segments would be
    /// a picture of nothing, and the page says so in prose instead.
    pub fn is_empty(&self) -> bool {
        self.slices.is_empty() || self.total.is_zero()
    }

    /// Where the money actually went — the first slice, since they are ranked.
    pub fn largest(&self) -> Option<&CostSlice> {
        self.slices.first()
    }
}

// -- the breakdown (gh#227) --------------------------------------------------

/// The ways one window can be cut apart.
///
/// One card with a toggle rather than three fixed columns of counts. The
/// question a reader brings is always the same — *which row is the bill* — and
/// only the axis they want it answered along changes between visits, so the
/// axis is a control and not a layout decision taken once for them.
///
/// Model leads because it is the axis where the answer is usually a single row,
/// and it is the one the shipped page could not ask at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Dimension {
    /// What the run said it was running.
    Model,
    /// Which harness ran it.
    Runtime,
    /// Which workspace it went to.
    Space,
    /// Which tracker the task came from.
    Tracker,
    /// Whose subscription it spent — [`BoardStats::by_account`]'s key.
    Account,
}

impl Dimension {
    /// Every dimension, in the order the toggle offers them.
    pub const ALL: [Dimension; 5] = [
        Dimension::Model,
        Dimension::Runtime,
        Dimension::Space,
        Dimension::Tracker,
        Dimension::Account,
    ];

    /// The word on the segment.
    pub fn label(self) -> &'static str {
        match self {
            Dimension::Model => "Model",
            Dimension::Runtime => "Runtime",
            Dimension::Space => "Space",
            Dimension::Tracker => "Tracker",
            Dimension::Account => "Account",
        }
    }
}

/// What a cut is ranked and scaled against.
///
/// Spend wherever the board could price the window, because that is the
/// question the card is opened with. Tokens where it has rates for nothing in
/// it, dispatches where nothing was metered at all — ranking rows by a number
/// every one of them holds at zero is alphabetical order with extra steps, and
/// a bar drawn against it is a row of empty tracks.
///
/// It is reported rather than inferred at the call site so the card can *say*
/// which quantity it sorted by: a bar whose meaning changes with the window is
/// one a reader has to be told about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Ranking {
    Spend,
    Tokens,
    Dispatches,
}

impl Ranking {
    /// How the card names it, beside its title.
    pub fn caption(self) -> &'static str {
        match self {
            Ranking::Spend => "by spend",
            Ranking::Tokens => "by tokens",
            Ranking::Dispatches => "by dispatches",
        }
    }
}

/// One row of the breakdown: a name, what it ran, what it spent, and what that
/// would have cost.
///
/// `cost: None` is **unpriced**, never zero — the rule the money type is built
/// on (gh#182), carried down to the row. A board with no rates configured
/// answers every row with `None` and the card drops the column rather than
/// printing a wall of `$0.00`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BreakdownRow {
    pub label: String,
    /// Attempts in this bucket — every attempt, not only the metered ones.
    pub dispatches: usize,
    /// What the metered ones spent.
    pub usage: TokenUsage,
    /// List price of that usage, priced per model so a bucket that ran two
    /// models is priced at both their rates rather than at an average.
    pub cost: Option<Usd>,
    /// Tokens in this row the rate table could not price, and which are
    /// therefore *not* in [`cost`](Self::cost). Carried rather than dropped:
    /// the same reason [`BoardSpend::unpriced`] exists.
    pub unpriced_tokens: u64,
}

impl BreakdownRow {
    /// What this row is ranked and drawn against.
    pub fn metric(&self, ranking: Ranking) -> u64 {
        match ranking {
            Ranking::Spend => self.cost.map_or(0, |c| c.micros.max(0) as u64),
            Ranking::Tokens => self.usage.total(),
            Ranking::Dispatches => self.dispatches as u64,
        }
    }
}

/// One cut of the window, ranked and ready to draw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Breakdown {
    pub dimension: Dimension,
    /// Biggest first by [`ranking`](Self::ranking), ties alphabetical, the tail
    /// folded into one honest `n others`.
    pub rows: Vec<BreakdownRow>,
    pub ranking: Ranking,
    /// The biggest row's metric — what the bars are scaled against, folded row
    /// included, because `n others` is a real bucket and not a footnote.
    pub peak: u64,
}

impl Breakdown {
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// One row's share of the biggest, `0.0..=1.0`.
    pub fn share(&self, row: &BreakdownRow) -> f32 {
        if self.peak == 0 {
            return 0.0;
        }
        (row.metric(self.ranking) as f32 / self.peak as f32).clamp(0.0, 1.0)
    }

    /// Is there a price to put in the last column at all? False on a board with
    /// no rates, which drops the column rather than filling it with dashes.
    pub fn is_priced(&self) -> bool {
        self.rows.iter().any(|r| r.cost.is_some())
    }
}

/// How many rows a cut shows before folding the rest into `n others`.
pub const BREAKDOWN_ROWS: usize = 6;

/// Rank one cut for reading (gh#227).
///
/// The same rule every tally here follows — biggest first, ties alphabetical so
/// an unchanged board redraws identically, the tail folded into one row that
/// carries what it stands for — applied to rows that hold three quantities
/// instead of one, against whichever of them this window can actually be ranked
/// on.
///
/// Like [`hour_grid`], this has no Swift counterpart yet: the phone's stats
/// screen draws no breakdown, so there is nothing there to disagree with it.
/// When it does, this is the arithmetic it adopts.
pub fn rank_breakdown(dimension: Dimension, mut rows: Vec<BreakdownRow>, max: usize) -> Breakdown {
    let ranking = if rows.iter().any(|r| r.cost.is_some_and(|c| !c.is_zero())) {
        Ranking::Spend
    } else if rows.iter().any(|r| !r.usage.is_zero()) {
        Ranking::Tokens
    } else {
        Ranking::Dispatches
    };
    rows.sort_by(|a, b| {
        b.metric(ranking)
            .cmp(&a.metric(ranking))
            .then_with(|| a.label.cmp(&b.label))
    });
    if max > 0 && rows.len() > max {
        let tail = rows.split_off(max);
        let priced: Vec<Usd> = tail.iter().filter_map(|r| r.cost).collect();
        rows.push(BreakdownRow {
            label: format!("{} others", tail.len()),
            dispatches: tail.iter().map(|r| r.dispatches).sum(),
            usage: tail.iter().map(|r| r.usage).sum(),
            // `None` only when not one folded row had a price. A fold that
            // reported zero for rows nobody could price would be the invented
            // zero this whole half of the page is designed against.
            cost: (!priced.is_empty()).then(|| priced.into_iter().sum()),
            unpriced_tokens: tail.iter().map(|r| r.unpriced_tokens).sum(),
        });
    }
    Breakdown {
        dimension,
        peak: rows.iter().map(|r| r.metric(ranking)).max().unwrap_or(0),
        ranking,
        rows,
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
    /// The same hours, split by the workspace they went to (gh#179) — the
    /// crossing [`hour_grid`] draws, and the reason "when" and "where" are one
    /// block rather than two cards that each hide what the other knows.
    ///
    /// Every value is [`HOURS`] long. Defaulted on the wire: a board that
    /// predates this field answers without it, and an absent crossing is a page
    /// that falls back to the margins rather than one that fails to decode.
    #[serde(default)]
    pub hours_by_workspace: BTreeMap<String, Vec<usize>>,

    pub by_workspace: BTreeMap<String, usize>,
    pub by_runtime: BTreeMap<String, usize>,
    pub by_source: BTreeMap<String, usize>,
    /// Whose subscription the attempts spent, by the login's own name.
    pub by_account: BTreeMap<String, usize>,

    /// The same window cut five ways, with tokens and money on every row
    /// (gh#227) — the breakdown card's whole contents.
    ///
    /// Ranked and folded on the box rather than on the page, because the money
    /// on a row cannot be derived from anything else here: a bucket's price is
    /// its own tokens at the rates of the models *it* ran, and only the box has
    /// the rate table. A dimension this window has nothing under is absent from
    /// the vector, which is how the toggle comes to omit it rather than offer a
    /// segment that opens onto nothing.
    ///
    /// Defaulted on the wire: a board that predates this field answers without
    /// it, and the card is then simply not drawn.
    #[serde(default)]
    pub breakdown: Vec<Breakdown>,
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
            hour_of_day: vec![0; HOURS],
            hours_by_workspace: BTreeMap::new(),
            by_workspace: BTreeMap::new(),
            by_runtime: BTreeMap::new(),
            by_source: BTreeMap::new(),
            by_account: BTreeMap::new(),
            breakdown: Vec::new(),
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

    /// The cut along one axis, or `None` when this window has nothing under it
    /// — which is the same thing as the toggle not offering it (gh#227).
    pub fn cut(&self, dimension: Dimension) -> Option<&Breakdown> {
        self.breakdown.iter().find(|b| b.dimension == dimension)
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

/// The crossing of *when* against *where*, ready to draw (gh#179).
///
/// The same ranking rule as every tally here — biggest first, ties
/// alphabetical, the tail folded into one honest `n others` row that carries
/// the hours it stands for — applied to a row of 24 numbers instead of one.
/// The column margin is summed after the fold, over every row, so the bottom
/// of the grid is the hour histogram it replaced and not just the rows that
/// survived the cap.
///
/// Rows arrive from the wire ([`BoardStats::hours_by_workspace`]) and are
/// normalised to [`HOURS`] on the way in: a board that answered with a shorter
/// vector is a board this one does not have to trust to index safely.
///
/// Unlike the rules beside it this one has no Swift counterpart yet — the
/// phone's stats screen (`apps/ios/Comet/Views/StatsView.swift`) draws no grid,
/// so there is nothing there to disagree with it. When it does, this is the
/// arithmetic it adopts, and the cross-language fixture is where the case goes.
pub fn hour_grid(hours_by_workspace: &BTreeMap<String, Vec<usize>>, max_rows: usize) -> HourGrid {
    let mut rows: Vec<HourRow> = hours_by_workspace
        .iter()
        .map(|(label, hours)| {
            let mut slots = vec![0usize; HOURS];
            for (slot, count) in slots.iter_mut().zip(hours.iter()) {
                *slot = *count;
            }
            HourRow {
                label: label.clone(),
                total: slots.iter().sum(),
                hours: slots,
            }
        })
        // A space with nothing in the window is noise in a grid about when
        // work happens — the same rule `ranked_tokens` applies to a model that
        // spent nothing.
        .filter(|row| row.total > 0)
        .collect();
    rows.sort_by(|a, b| b.total.cmp(&a.total).then_with(|| a.label.cmp(&b.label)));

    if max_rows > 0 && rows.len() > max_rows {
        let tail = rows.split_off(max_rows);
        let mut folded = vec![0usize; HOURS];
        for row in &tail {
            for (slot, count) in folded.iter_mut().zip(row.hours.iter()) {
                *slot += count;
            }
        }
        rows.push(HourRow {
            label: format!("{} others", tail.len()),
            total: folded.iter().sum(),
            hours: folded,
        });
    }

    let mut columns = vec![0usize; HOURS];
    for row in &rows {
        for (slot, count) in columns.iter_mut().zip(row.hours.iter()) {
            *slot += count;
        }
    }
    HourGrid {
        peak: rows
            .iter()
            .flat_map(|r| r.hours.iter().copied())
            .max()
            .unwrap_or(0),
        total: rows.iter().map(|r| r.total).sum(),
        rows,
        columns,
    }
}

/// A ratio said the way a person compares two prices: `12×`, `5.3×`, `0.42×`.
///
/// Precision grows as the number shrinks, because that is where it starts to
/// matter: nobody acts on the difference between 12.3× and 12× of subsidy, and
/// everybody acts on the difference between 0.9× and 0.4×.
pub fn human_multiple(ratio: f64) -> String {
    let r = ratio.max(0.0);
    if r >= 10.0 {
        format!("{r:.0}×")
    } else if r >= 1.0 {
        format!("{r:.1}×")
    } else {
        format!("{r:.2}×")
    }
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

/// [`bar_fraction`] for a token count, which needs the whole `u64` range: a
/// week can put nine figures through one bar.
pub fn token_fraction(value: u64, peak: u64) -> f32 {
    if peak == 0 {
        return 0.0;
    }
    (value as f64 / peak as f64).clamp(0.0, 1.0) as f32
}

/// The busiest bucket in a day series — still what the dispatch counts are
/// scaled against wherever one is drawn (the phone's chart, until gh#181).
pub fn peak_dispatches(daily: &[DayBucket]) -> usize {
    daily.iter().map(|d| d.dispatches).max().unwrap_or(0)
}

/// The same, for the token series — the scale the desktop day chart is drawn
/// against, and the figure its peak annotation reads out (gh#226).
pub fn peak_tokens(daily: &[TokenDay]) -> u64 {
    daily.iter().map(|d| d.usage.total()).max().unwrap_or(0)
}

/// A bucket date as a chart caption says it: `Mon 3`.
///
/// Weekday and day-of-month, not the ISO date: under a bar, `2026-08-03` is ten
/// characters of which two are news, and the weekday is the half of the date a
/// reader is actually pattern-matching on. A date that will not parse is
/// returned as it came rather than guessed at.
pub fn short_day(date: &str) -> String {
    use chrono::Datelike as _;
    match chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d") {
        Ok(day) => format!("{} {}", day.format("%a"), day.day()),
        Err(_) => date.to_string(),
    }
}

/// How many columns a day chart can caption before the labels collide.
///
/// A week fits comfortably and a month does not — at eleven pixels a caption is
/// about fifty wide, and thirty of those want a chart no window is. Past this
/// the columns are drawn bare and the axis under them carries the range, which
/// is the shape a month is read for anyway.
pub const CAPTIONED_COLUMNS: usize = 10;

/// Whether a chart of `columns` days can carry a caption under every bar.
pub fn day_captions_fit(columns: usize) -> bool {
    columns <= CAPTIONED_COLUMNS
}

/// The day chart, ready to draw (gh#226): one column per day in the window,
/// oldest first, quiet days included.
///
/// Driven by the dispatch series because that is the one built from the
/// window's whole calendar; the tokens are looked up by date rather than by
/// index, so two series that disagreed in length would draw short rather than
/// draw a spike under the wrong day.
pub fn day_columns(daily: &[DayBucket], daily_tokens: &[TokenDay]) -> Vec<DayColumn> {
    let peak = peak_tokens(daily_tokens);
    let spent: BTreeMap<&str, u64> = daily_tokens
        .iter()
        .map(|d| (d.date.as_str(), d.usage.total()))
        .collect();
    daily
        .iter()
        .map(|day| {
            let tokens = spent.get(day.date.as_str()).copied().unwrap_or(0);
            DayColumn {
                date: day.date.clone(),
                tokens,
                dispatches: day.dispatches,
                fraction: token_fraction(tokens, peak),
                // An em dash, never `0` — the same rule the totals follow. A
                // day that spent nothing has no figure to show, and a zero
                // printed above a hairline is a number nobody needs to read.
                value: if tokens == 0 {
                    "—".to_string()
                } else {
                    human_tokens(tokens)
                },
                caption: format!("{} · {}", short_day(&day.date), day.dispatches),
            }
        })
        .collect()
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

    // -- the crossing (gh#179) -----------------------------------------------

    /// A workspace's day, from `(hour, count)` pairs.
    fn hours(pairs: &[(usize, usize)]) -> Vec<usize> {
        let mut out = vec![0usize; HOURS];
        for (hour, count) in pairs {
            out[*hour] = *count;
        }
        out
    }

    #[test]
    fn the_crossing_keeps_both_margins_the_two_cards_used_to_be() {
        let crossed: BTreeMap<String, Vec<usize>> = [
            ("comet".to_string(), hours(&[(9, 3), (22, 5)])),
            ("edge".to_string(), hours(&[(9, 2)])),
        ]
        .into_iter()
        .collect();
        let grid = hour_grid(&crossed, 6);
        // The workspace tally, as row totals: biggest first.
        assert_eq!(grid.rows[0].label, "comet");
        assert_eq!(grid.rows[0].total, 8);
        assert_eq!(grid.rows[1].total, 2);
        // The hour histogram, as the column margin.
        assert_eq!(grid.columns[9], 5);
        assert_eq!(grid.columns[22], 5);
        assert_eq!(grid.columns.iter().sum::<usize>(), 10);
        assert_eq!(grid.total, 10);
        // And the fact neither margin holds: the 22:00 releases are all one
        // space, the 09:00 ones are split.
        assert_eq!(grid.rows[0].hours[22], 5);
        assert_eq!(grid.rows[1].hours[22], 0);
        // Heat scales against the busiest CELL, not the busiest row.
        assert_eq!(grid.peak, 5);
    }

    #[test]
    fn a_capped_crossing_folds_the_tail_hour_by_hour() {
        let crossed: BTreeMap<String, Vec<usize>> = [
            ("a".to_string(), hours(&[(1, 9)])),
            ("b".to_string(), hours(&[(2, 4)])),
            ("c".to_string(), hours(&[(2, 3)])),
            ("d".to_string(), hours(&[(3, 2)])),
        ]
        .into_iter()
        .collect();
        let grid = hour_grid(&crossed, 2);
        assert_eq!(grid.rows.len(), 3);
        assert_eq!(grid.rows[2].label, "2 others");
        // Folded element-wise: the tail's two spaces both released in hour 2
        // and 3, and the fold keeps the hours rather than only the count.
        assert_eq!(grid.rows[2].hours[2], 3);
        assert_eq!(grid.rows[2].hours[3], 2);
        assert_eq!(grid.rows[2].total, 5);
        // The margin still totals every dispatch, folded ones included.
        assert_eq!(grid.columns.iter().sum::<usize>(), 18);
    }

    #[test]
    fn a_crossing_with_nothing_in_it_is_empty_rather_than_a_grid_of_zeroes() {
        assert!(hour_grid(&BTreeMap::new(), 6).is_empty());
        let quiet: BTreeMap<String, Vec<usize>> = [("comet".to_string(), vec![0; HOURS])]
            .into_iter()
            .collect();
        let grid = hour_grid(&quiet, 6);
        assert!(grid.is_empty());
        assert_eq!(grid.peak, 0);
    }

    #[test]
    fn a_short_row_off_the_wire_still_has_a_slot_for_every_hour() {
        // An older board, or one that answered with a truncated series: the
        // grid indexes 24 slots either way rather than trusting the sender.
        let ragged: BTreeMap<String, Vec<usize>> =
            [("comet".to_string(), vec![1, 2, 3])].into_iter().collect();
        let grid = hour_grid(&ragged, 6);
        assert_eq!(grid.rows[0].hours.len(), HOURS);
        assert_eq!(grid.rows[0].total, 6);
        assert_eq!(grid.columns.len(), HOURS);
    }

    // -- the breakdown (gh#227) ----------------------------------------------

    fn row(label: &str, dispatches: usize, tokens: u64, dollars: Option<f64>) -> BreakdownRow {
        BreakdownRow {
            label: label.to_string(),
            dispatches,
            usage: TokenUsage {
                input_tokens: tokens,
                ..TokenUsage::default()
            },
            cost: dollars.map(Usd::from_dollars),
            unpriced_tokens: 0,
        }
    }

    #[test]
    fn a_priced_cut_ranks_on_the_money_and_scales_its_bars_against_it() {
        let cut = rank_breakdown(
            Dimension::Model,
            vec![
                row("gpt-5.6-terra", 4, 7_400_000, Some(38.0)),
                row("claude-opus-5", 9, 31_200_000, Some(198.0)),
                row("claude-sonnet-5", 2, 3_400_000, Some(14.0)),
            ],
            BREAKDOWN_ROWS,
        );
        assert_eq!(cut.ranking, Ranking::Spend);
        let labels: Vec<&str> = cut.rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            ["claude-opus-5", "gpt-5.6-terra", "claude-sonnet-5"]
        );
        // The bar is the same quantity the rows are sorted by, or it is a
        // second ordering drawn on top of the first.
        assert_eq!(cut.share(&cut.rows[0]), 1.0);
        assert!((cut.share(&cut.rows[1]) - 38.0 / 198.0).abs() < 0.001);
        assert!(cut.is_priced());
    }

    #[test]
    fn an_unpriced_cut_falls_back_to_tokens_and_then_to_dispatches() {
        let tokens = rank_breakdown(
            Dimension::Runtime,
            vec![
                row("claude-code", 3, 900, None),
                row("codex", 9, 40_000, None),
            ],
            BREAKDOWN_ROWS,
        );
        assert_eq!(tokens.ranking, Ranking::Tokens);
        assert_eq!(tokens.rows[0].label, "codex", "tokens, not dispatches");
        assert!(!tokens.is_priced(), "no rates is not a column of zeroes");

        // Nothing metered at all: the only number left is how often it ran.
        let counted = rank_breakdown(
            Dimension::Tracker,
            vec![row("github", 2, 0, None), row("linear", 7, 0, None)],
            BREAKDOWN_ROWS,
        );
        assert_eq!(counted.ranking, Ranking::Dispatches);
        assert_eq!(counted.rows[0].label, "linear");
        assert_eq!(counted.share(&counted.rows[0]), 1.0);
    }

    /// Rates that priced *nothing* is not the same as no rates: the rows carry
    /// a real zero and the tokens are still what orders them.
    #[test]
    fn a_cut_priced_at_zero_is_ranked_on_what_it_actually_spent() {
        let cut = rank_breakdown(
            Dimension::Space,
            vec![
                BreakdownRow {
                    unpriced_tokens: 900,
                    ..row("edge", 3, 900, Some(0.0))
                },
                BreakdownRow {
                    unpriced_tokens: 40_000,
                    ..row("comet", 2, 40_000, Some(0.0))
                },
            ],
            BREAKDOWN_ROWS,
        );
        assert_eq!(cut.ranking, Ranking::Tokens);
        assert_eq!(cut.rows[0].label, "comet");
    }

    #[test]
    fn a_folded_cut_carries_every_quantity_it_stood_for() {
        let cut = rank_breakdown(
            Dimension::Account,
            (1..=5)
                .map(|n| row(&format!("a{n}"), n, n as u64 * 1_000, Some(n as f64)))
                .collect(),
            2,
        );
        assert_eq!(cut.rows.len(), 3);
        let folded = &cut.rows[2];
        assert_eq!(folded.label, "3 others");
        assert_eq!(folded.dispatches, 1 + 2 + 3);
        assert_eq!(folded.usage.total(), 6_000);
        assert_eq!(folded.cost, Some(Usd::from_dollars(6.0)));
        // Every dollar the cut was given is still in it after the fold.
        assert_eq!(
            cut.rows.iter().filter_map(|r| r.cost).sum::<Usd>(),
            Usd::from_dollars(15.0)
        );
        // And `n others` is a bucket like any other: it scales the bars when it
        // is the biggest one.
        assert_eq!(cut.peak, folded.cost.expect("priced").micros as u64);
        assert_eq!(cut.share(folded), 1.0);
    }

    #[test]
    fn a_dimension_with_nothing_under_it_is_absent_rather_than_empty() {
        let mut s = BoardStats::empty(Some(7));
        assert_eq!(s.cut(Dimension::Model), None);
        s.breakdown = vec![rank_breakdown(
            Dimension::Runtime,
            vec![row("claude-code", 1, 10, None)],
            BREAKDOWN_ROWS,
        )];
        assert_eq!(s.cut(Dimension::Model), None, "not a segment on the toggle");
        assert!(s.cut(Dimension::Runtime).is_some());
        // The toggle's order is the type's, not the vector's.
        assert_eq!(Dimension::ALL[0].label(), "Model");
        assert_eq!(Dimension::ALL.len(), 5);
    }

    #[test]
    fn a_multiple_gets_precise_where_precision_starts_to_matter() {
        assert_eq!(human_multiple(12.34), "12×");
        assert_eq!(human_multiple(5.28), "5.3×");
        assert_eq!(human_multiple(1.0), "1.0×");
        // Under one: the plan cost more than the work would have.
        assert_eq!(human_multiple(0.4231), "0.42×");
        assert_eq!(human_multiple(0.0), "0.00×");
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
            in_flight: 2,
        };
        assert_eq!(l.total(), 15);
        // The bar is over landed work; the two still running are a caption.
        assert_eq!(l.touched(), 17);
        assert_eq!(l.headline(), "15 tasks");
        assert_eq!(
            l.in_flight_note().as_deref(),
            Some("2 still running — not landed anywhere yet")
        );
    }

    #[test]
    fn the_bar_keeps_the_two_categories_a_merge_count_hides() {
        let l = Landing {
            merged: 9,
            open: 2,
            closed_unmerged: 1,
            no_pr: 3,
            in_flight: 2,
        };
        let segments = l.segments();
        // Four bands, best first, and the shares are of what landed — the two
        // still running must not shrink the merged band.
        assert_eq!(segments.len(), 4);
        assert_eq!(
            segments.iter().map(|s| s.count).collect::<Vec<_>>(),
            vec![9, 2, 1, 3]
        );
        assert!((segments.iter().map(|s| s.fraction).sum::<f64>() - 1.0).abs() < 1e-9);
        assert!((segments[0].fraction - 9.0 / 15.0).abs() < 1e-9);
        assert_eq!(
            segments
                .iter()
                .map(|s| s.label.as_str())
                .collect::<Vec<_>>(),
            vec!["Merged", "PR open", "Closed unmerged", "No PR raised"]
        );
    }

    #[test]
    fn a_window_that_lost_nothing_still_says_so() {
        // The whole point of gh#228: `Closed unmerged 0` is a fact, and a
        // legend that drops it is one a reader cannot distinguish from a
        // surface that never counted losses.
        let clean = Landing {
            merged: 4,
            ..Default::default()
        };
        let segments = clean.segments();
        assert_eq!(segments.len(), 4);
        assert_eq!(segments[2].count, 0);
        assert_eq!(segments[2].fraction, 0.0);
        assert_eq!(clean.in_flight_note(), None);

        // Nothing landed at all: four zero bands and no division by nothing.
        let empty = Landing {
            in_flight: 1,
            ..Default::default()
        };
        assert_eq!(empty.headline(), "0 tasks");
        assert!(empty.segments().iter().all(|s| s.fraction == 0.0));
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

    // -- the day chart (gh#226) ----------------------------------------------

    fn bucket(date: &str, dispatches: usize, done: usize) -> DayBucket {
        DayBucket {
            date: date.to_string(),
            dispatches,
            done,
        }
    }

    fn spent_on(date: &str, tokens: u64) -> TokenDay {
        TokenDay {
            date: date.to_string(),
            usage: usage(tokens, 0, 0, 0),
        }
    }

    #[test]
    fn a_day_column_is_as_tall_as_what_it_spent_and_says_what_it_ran() {
        // Two dispatches and 3.1M tokens on the Monday, three dispatches and a
        // twentieth of that on the Tuesday: the count is nearly flat and the
        // volume is not, which is the whole reason the bar plots the second.
        let daily = vec![bucket("2026-08-03", 2, 2), bucket("2026-08-04", 3, 1)];
        let tokens = vec![
            spent_on("2026-08-03", 3_100_000),
            spent_on("2026-08-04", 155_000),
        ];
        let columns = day_columns(&daily, &tokens);
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0].value, "3.10M");
        assert_eq!(columns[0].caption, "Mon 3 · 2");
        assert_eq!(columns[0].fraction, 1.0, "the busiest day is the scale");
        assert_eq!(columns[1].value, "155k");
        assert_eq!(columns[1].caption, "Tue 4 · 3");
        assert_eq!(columns[1].fraction, 0.05);
        assert!(columns.iter().all(|c| !c.is_quiet()));
    }

    /// The failure this chart is drawn against: a week where one day worked
    /// must read as one busy day in seven, not as a single lonely bar.
    #[test]
    fn a_quiet_day_is_a_column_with_a_dash_over_it_and_not_an_absent_one() {
        let daily = vec![
            bucket("2026-08-03", 2, 2),
            bucket("2026-08-04", 0, 0),
            // Dispatched and never metered: still a quiet column, because the
            // bar plots tokens — and the caption still says two ran.
            bucket("2026-08-05", 2, 1),
        ];
        let columns = day_columns(&daily, &[spent_on("2026-08-03", 40_000)]);
        assert_eq!(columns.len(), 3, "every day in the window draws");
        assert!(!columns[0].is_quiet());
        assert!(columns[1].is_quiet());
        assert_eq!(columns[1].value, "—", "a dash, never a zero");
        assert_eq!(columns[1].caption, "Tue 4 · 0");
        assert_eq!(columns[1].fraction, 0.0);
        assert!(columns[2].is_quiet());
        assert_eq!(
            columns[2].caption, "Wed 5 · 2",
            "unmetered work still says what ran"
        );
    }

    #[test]
    fn a_window_that_metered_nothing_still_draws_its_days() {
        // No peak to scale against is not a reason to draw nothing: the days
        // are the shape, and the coverage line above says why they are flat.
        let daily = vec![bucket("2026-08-03", 1, 1), bucket("2026-08-04", 2, 0)];
        let columns = day_columns(&daily, &[]);
        assert_eq!(columns.len(), 2);
        assert!(columns.iter().all(|c| c.is_quiet() && c.fraction == 0.0));
        assert_eq!(columns[1].caption, "Tue 4 · 2");
        assert!(day_columns(&[], &[]).is_empty());
    }

    #[test]
    fn a_column_takes_its_tokens_by_date_and_not_by_position() {
        // A board that answered with a shorter token series must not put the
        // spike under the wrong day.
        let daily = vec![
            bucket("2026-08-03", 1, 1),
            bucket("2026-08-04", 1, 1),
            bucket("2026-08-05", 1, 1),
        ];
        let columns = day_columns(&daily, &[spent_on("2026-08-05", 900)]);
        assert_eq!(columns[0].tokens, 0);
        assert_eq!(columns[2].tokens, 900);
        assert_eq!(columns[2].value, "900");
    }

    #[test]
    fn a_bar_scales_over_the_whole_token_range() {
        // Nine figures through one bar is an ordinary week, and the `usize`
        // rule beside this one would have to round them.
        assert_eq!(token_fraction(0, 0), 0.0);
        assert_eq!(token_fraction(5, 0), 0.0, "no peak is no bar, not a crash");
        assert_eq!(token_fraction(1_000_000_000, 4_000_000_000), 0.25);
        assert_eq!(token_fraction(9, 4), 1.0, "clamped, never overdrawn");
    }

    #[test]
    fn a_caption_is_a_weekday_and_a_date_that_will_not_parse_is_left_alone() {
        assert_eq!(short_day("2026-08-03"), "Mon 3");
        assert_eq!(short_day("2026-12-25"), "Fri 25");
        assert_eq!(short_day("whenever"), "whenever");
    }

    #[test]
    fn a_month_of_columns_goes_bare_rather_than_illegible() {
        assert!(day_captions_fit(1));
        assert!(day_captions_fit(7), "the default window is captioned");
        assert!(day_captions_fit(CAPTIONED_COLUMNS));
        assert!(!day_captions_fit(30), "a month reads as a shape");
    }

    // -- the spend headline (gh#182 rendered by gh#179) -----------------------

    fn account(
        label: &str,
        list: f64,
        monthly: Option<f64>,
        in_window: Option<f64>,
    ) -> AccountSpend {
        AccountSpend {
            label: label.to_string(),
            attempts: 1,
            usage: usage(1, 1, 0, 0),
            list_price: Usd::from_dollars(list),
            unpriced_tokens: 0,
            plan: monthly.map(|m| AccountPlan {
                label: None,
                monthly: Usd::from_dollars(m),
            }),
            plan_in_window: in_window.map(Usd::from_dollars),
        }
    }

    fn priced(list: f64, accounts: Vec<AccountSpend>) -> BoardSpend {
        BoardSpend {
            rates: crate::view::rates::RateTable::empty("2026-06-24"),
            list_price: Usd::from_dollars(list),
            by_model: Vec::new(),
            unpriced: Vec::new(),
            unpriced_tokens: 0,
            accounts,
        }
    }

    #[test]
    fn the_subsidy_is_a_ratio_of_the_window_and_never_a_sum() {
        let spend = priced(
            240.0,
            vec![
                account("brede@tally.no", 200.0, Some(200.0), Some(46.666_667)),
                account("ana@example.com", 40.0, Some(20.0), Some(4.666_667)),
            ],
        );
        // The two plans are pro-rated onto the same window and compared with
        // the list price — never added to it.
        let plans = spend.subscriptions_in_window().expect("both have plans");
        assert_eq!(plans, Usd::from_dollars(51.333_334));
        assert_eq!(human_multiple(spend.subsidy().expect("a ratio")), "4.7×");
        // The monthly figure is a different question and stays whole.
        assert_eq!(spend.monthly_subscriptions(), Usd::from_dollars(220.0));
    }

    #[test]
    fn a_window_with_no_plan_entered_has_no_multiple_rather_than_an_infinite_one() {
        // Unentered is not free: with nothing to divide by there is no answer,
        // and the page says so instead of drawing a subsidy of ∞.
        let spend = priced(12.4, vec![account("brede@tally.no", 12.4, None, None)]);
        assert_eq!(spend.subscriptions_in_window(), None);
        assert_eq!(spend.subsidy(), None);
        // A plan that costs nothing is the same non-answer.
        let free = priced(
            12.4,
            vec![account("ci@example.com", 12.4, Some(0.0), Some(0.0))],
        );
        assert_eq!(free.subscriptions_in_window(), Some(Usd::ZERO));
        assert_eq!(free.subsidy(), None);
    }

    // -- where the list price goes (gh#225) ----------------------------------

    /// One priced model, at a rate written the way the table writes it.
    fn model(label: &str, input: f64, output: f64, usage: TokenUsage) -> ModelSpend {
        let rate = ModelRate::published(input, output);
        ModelSpend {
            label: label.to_string(),
            rate_key: label.to_string(),
            source: RateSource::Builtin,
            rate,
            usage,
            cost: rate.cost(usage),
        }
    }

    fn split_of(models: Vec<ModelSpend>) -> BoardSpend {
        BoardSpend {
            rates: crate::view::rates::RateTable::empty("2026-06-24"),
            list_price: models.iter().map(|m| m.cost).sum(),
            by_model: models,
            unpriced: Vec::new(),
            unpriced_tokens: 0,
            accounts: Vec::new(),
        }
    }

    #[test]
    fn the_split_ranks_on_money_and_adds_up_to_the_price_it_splits() {
        // A week shaped like a coding agent's: cached input dwarfs everything
        // in tokens, and output costs the most anyway. That inversion is the
        // whole reason the block exists.
        let spend = split_of(vec![
            model(
                "claude-opus-5",
                5.0,
                25.0,
                usage(2_000_000, 1_000_000, 30_000_000, 3_000_000),
            ),
            model(
                "claude-haiku-4-5",
                1.0,
                5.0,
                usage(100_000, 240_000, 4_800_000, 900_000),
            ),
        ]);
        let split = spend.cost_split();
        assert!(!split.is_empty());
        // Biggest spend first, and the reader's ordering is not the token one.
        let labels: Vec<&str> = split.slices.iter().map(|s| s.label()).collect();
        assert_eq!(
            labels,
            ["output", "cache writes", "cached input", "uncached input"]
        );
        assert_eq!(split.largest().expect("a biggest class").label(), "output");
        // The same terms `ModelRate::cost` sums, kept apart — so the split is
        // the list price exactly, not nearly. A breakdown that did not add up
        // to the figure above it would be the one thing this page must not do.
        assert_eq!(split.total, spend.list_price);
        assert_eq!(
            split.slices.iter().map(|s| s.cost).sum::<Usd>(),
            spend.list_price
        );
        // And the tokens are the tokens, both readings of them.
        assert_eq!(
            split.tokens,
            spend.by_model.iter().map(|m| m.usage.total()).sum::<u64>()
        );
        let cached = split
            .slices
            .iter()
            .find(|s| s.class == CostClass::CacheRead)
            .expect("cached input");
        assert_eq!(cached.tokens, 34_800_000);
        assert_eq!(cached.legend(), "cached input $15.48 / 34.80M");
        // Shares are of the priced total and cover it.
        let shares: f64 = split.slices.iter().map(|s| s.share).sum();
        assert!((shares - 1.0).abs() < 1e-9, "{shares}");
    }

    #[test]
    fn a_class_nobody_spent_in_is_absent_rather_than_an_empty_segment() {
        // A harness that reports no cache at all: two classes, not four with
        // two of them drawn as slivers of nothing.
        let spend = split_of(vec![model(
            "gpt-5.6-luna",
            2.0,
            10.0,
            usage(500_000, 200_000, 0, 0),
        )]);
        let split = spend.cost_split();
        assert_eq!(split.slices.len(), 2);
        assert!(split.slices.iter().all(|s| s.tokens > 0));
        assert_eq!(split.total, spend.list_price);
    }

    #[test]
    fn a_window_with_nothing_priced_has_no_bar_to_draw() {
        // The empty state the block collapses to prose for: nothing priced, and
        // a rate table full of zeroes, which are two ways to have no picture.
        assert!(split_of(Vec::new()).cost_split().is_empty());
        let free = split_of(vec![model(
            "local-llama",
            0.0,
            0.0,
            usage(1_000, 500, 0, 0),
        )]);
        let split = free.cost_split();
        assert!(split.is_empty(), "a price of zero is not a shape");
        // The tokens are still real, and still counted — it is the *money* that
        // has no shape here.
        assert_eq!(split.tokens, 1_500);
        assert_eq!(split.largest().map(|s| s.label()), Some("output"));
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
            in_flight: 1,
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
        s.hours_by_workspace
            .insert("attn".into(), hours(&[(9, 3), (22, 1)]));
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
        assert!(json.get("hoursByWorkspace").is_some());
        assert!(json.get("byWorkspace").is_some());
        assert!(json.get("agentDispatched").is_some());
        assert!(json["landing"].get("closedUnmerged").is_some());
        assert!(json["landing"].get("noPr").is_some());
        assert!(json["landing"].get("inFlight").is_some());
        assert!(json["friction"].get("retriedTasks").is_some());
        assert!(json["friction"].get("blockedEntries").is_some());
        assert!(json["daily"][0].get("dispatches").is_some());
        assert!(json.get("attemptsWithTokens").is_some());
        assert!(json.get("tokenCoverage").is_some());
        assert!(json.get("tokensByModel").is_some());
        assert!(json.get("tokensByRuntime").is_some());
        assert_eq!(json["tokens"]["cacheReadTokens"], 40_000);
        assert_eq!(
            json["dailyTokens"][0]["usage"]["cacheCreationTokens"],
            3_000
        );

        let back: BoardStats = serde_json::from_value(json.clone()).expect("deserializes");
        assert_eq!(back, s);

        // A board that predates the crossing (gh#179) answers without the key,
        // and its throughput numbers still arrive: an older box is a page with
        // no grid, not a page that failed to decode.
        let mut older = json;
        older
            .as_object_mut()
            .expect("an object")
            .remove("hoursByWorkspace");
        let back: BoardStats =
            serde_json::from_value(older.clone()).expect("deserializes without it");
        assert!(back.hours_by_workspace.is_empty());
        assert_eq!(back.attempts, 4);

        // Same for a board that predates the in-flight split (gh#228): the
        // four landed categories still arrive, and nothing is reported as
        // running.
        older["landing"]
            .as_object_mut()
            .expect("an object")
            .remove("inFlight");
        let back: BoardStats = serde_json::from_value(older).expect("deserializes without it");
        assert_eq!(back.landing.in_flight, 0);
        assert_eq!(back.landing.merged, 2);
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
                // gh#228. The bar both viewports draw: four bands, the two
                // losses among them, and shares taken over what landed rather
                // than over what was touched.
                "landingHeadline": stats.landing.headline(),
                "landingSegments": stats.landing.segments(),
                "landingInFlightNote": stats.landing.in_flight_note(),
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
        // Nine landed and two still going, which is the eleven tasks the window
        // touched. The two live attempts above are on tasks that have raised
        // nothing yet: touched by the window, landed nowhere in it.
        busy.landing = Landing {
            merged: 5,
            open: 2,
            closed_unmerged: 1,
            no_pr: 1,
            in_flight: 2,
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
        // The crossing (gh#179), summing to the tally above it hour by hour:
        // one space worked on all evening, one only in the afternoon. The
        // phone does not draw this yet and ignores the key — what the fixture
        // pins here is that it still decodes the rest of the reply when a
        // newer board sends it.
        busy.hours_by_workspace = [
            ("comet-native", &[(14, 2), (21, 3), (22, 4)][..]),
            ("edge", &[(14, 3), (15, 1)][..]),
        ]
        .into_iter()
        .map(|(label, pairs)| {
            let mut slots = vec![0usize; HOURS];
            for (hour, count) in pairs {
                slots[*hour] = *count;
            }
            (label.to_string(), slots)
        })
        .collect();
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
        // The same window cut two ways (gh#227), ranked and folded on the box.
        // The phone draws no breakdown yet and ignores the key — what the
        // fixture pins here is that it still decodes the rest of the reply when
        // a newer board sends one, and what the shape it will read looks like.
        priced.breakdown = vec![
            rank_breakdown(
                Dimension::Model,
                vec![
                    BreakdownRow {
                        label: "claude-opus-5".into(),
                        dispatches: 8,
                        usage: usage(9_000, 6_000, 148_000, 21_000),
                        cost: Some(Usd::from_dollars(0.400_25)),
                        unpriced_tokens: 0,
                    },
                    BreakdownRow {
                        label: "gpt-5.6-terra".into(),
                        dispatches: 5,
                        usage: usage(400, 100, 0, 500),
                        // Metered, and priced at nothing because the table has
                        // never heard of it — which is why the tokens it could
                        // not price ride along on the row.
                        cost: Some(Usd::ZERO),
                        unpriced_tokens: 1_000,
                    },
                ],
                BREAKDOWN_ROWS,
            ),
            rank_breakdown(
                Dimension::Runtime,
                vec![
                    BreakdownRow {
                        label: "claude-code".into(),
                        dispatches: 10,
                        usage: usage(9_000, 6_000, 148_000, 21_000),
                        cost: Some(Usd::from_dollars(0.400_25)),
                        unpriced_tokens: 0,
                    },
                    BreakdownRow {
                        label: "codex".into(),
                        dispatches: 3,
                        usage: usage(400, 100, 0, 500),
                        cost: Some(Usd::ZERO),
                        unpriced_tokens: 1_000,
                    },
                ],
                BREAKDOWN_ROWS,
            ),
        ];
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
