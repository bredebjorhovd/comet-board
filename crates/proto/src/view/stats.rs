//! What the board knows about its own throughput, as every surface reads it.
//!
//! The numbers are gathered by `comet_board::stats` on each host that owns a
//! `board.db`; the *shape* lives here so a viewport can deserialize either one
//! [`BoardStats`] or their explicit [`AggregateBoardStats`] union without
//! depending on the board crate — the same split
//! [`super::board::RuntimeOption`] makes, and for the same reason: the phone
//! and the laptop asking remote boxes for stats must not have to link a SQLite
//! store to read the answer.
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

use crate::view::rates::{ModelRate, RateSource, RateTable, Usd, human_usd};
use crate::{AgentKind, TokenUsage};

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

/// Semantic label for every dollar-valued estimate in [`BoardStats`] JSON.
///
/// The legacy field names (`listPrice`, `cost`) remain for wire compatibility;
/// this discriminator makes explicit that none of them is a bill. Older boxes
/// omit it and deserialize to the same only-supported basis.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PricingBasis {
    #[default]
    ListPriceApiEstimate,
}

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
    /// Legacy-compatible wire name. Its semantics are declared by
    /// [`BoardStats::pricing_basis`].
    pub cost: Usd,
}

/// One agent/model slice of the window, with its list-price API estimate
/// (gh#426).
///
/// These rows are present only for attempts whose harness emitted per-message
/// attribution. They are therefore read beside
/// [`BoardStats::attempts_with_agent_usage`], never as a complete rewrite of
/// the window total. `list_price_api_estimate: None` is rates not configured; `Some(0)` with
/// `unpriced_tokens > 0` is an unknown model, never free work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSpend {
    pub agent: AgentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub model: String,
    pub usage: TokenUsage,
    /// What this slice's usage would cost at public API list prices. This is
    /// deliberately verbose on the JSON wire: subscription runs are not billed
    /// per token, and a bare `cost` field would imply otherwise.
    pub list_price_api_estimate: Option<Usd>,
    pub unpriced_tokens: u64,
}

impl AgentSpend {
    /// Compact, stable row label shared by the CLI and viewports.
    pub fn label(&self) -> String {
        let agent = match self.agent {
            AgentKind::Main => "Main",
            AgentKind::Subagent => self.name.as_deref().unwrap_or("Subagent"),
        };
        format!("{agent} · {}", self.model)
    }

    /// Money column wording. No configured rates stays blank at the data
    /// level; an unknown model says `unpriced` instead of inventing `$0.00`.
    pub fn price_label(&self) -> Option<String> {
        self.list_price_api_estimate.map(|cost| {
            if self.unpriced_tokens > 0 && cost.is_zero() {
                UNPRICED.to_string()
            } else {
                human_usd(cost)
            }
        })
    }
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
    /// List-price API estimate of this account's metered work. Unpriced models
    /// are excluded and counted in [`unpriced_tokens`](Self::unpriced_tokens).
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
    /// List-price API estimate of everything the board could price in this
    /// window. The compatible JSON field remains `listPrice`; the response's
    /// [`BoardStats::pricing_basis`] states its semantics explicitly.
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

    /// The headline, said once: `$12.40 list-price API estimate`, with what it could not
    /// account for attached rather than left implied.
    pub fn headline(&self) -> String {
        let price = human_usd(self.list_price);
        if self.is_complete() {
            format!("{price} list-price API estimate")
        } else {
            format!(
                "{price} list-price API estimate, plus {} unpriced token(s) across {} model(s)",
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
            Ranking::Spend => "by list-price API estimate",
            Ranking::Tokens => "by tokens",
            Ranking::Dispatches => "by dispatches",
        }
    }
}

/// What the money column says about a row nobody could price (gh#359).
///
/// The word, not `$0.00` — which is a different and false claim — and not a
/// blank, which reads as a rendering bug. One spelling, here, because both
/// viewports write it and a phone that said `no rate` while the laptop said
/// `unpriced` would be two answers to one question.
pub const UNPRICED: &str = "unpriced";

/// What the money column says about a row that metered nothing at all: an em
/// dash. A bucket with dispatches and no usage has no money to report and no
/// tokens to report it against, which is a third fact again.
pub const NO_FIGURE: &str = "—";

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
    /// List-price API estimate of that usage, priced per model so a bucket that
    /// ran two models is priced at both their rates rather than at an average.
    /// The compatible JSON field remains `cost`; see
    /// [`BoardStats::pricing_basis`].
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

    /// Tokens here the table could not price, and no money at all to put in
    /// their place (gh#359) — a row that ran a model nobody has a rate for.
    ///
    /// Not the same as a row that metered nothing, and not the same as a row
    /// that ran one priced model and one unpriced one: that second row has a
    /// real figure to show, and the card's footer says what the figure leaves
    /// out. This is the row where the whole answer would otherwise be blank.
    pub fn is_unpriced(&self) -> bool {
        self.unpriced_tokens > 0 && self.cost.is_some_and(|c| c.is_zero())
    }

    /// What goes in the money column — three different facts, and never a zero
    /// somebody could act on.
    ///
    /// `unpriced` where there are tokens and no rate to price them at,
    /// [`NO_FIGURE`] where the bucket metered nothing, and the money otherwise.
    /// A board with no rates at all answers `None` for every row and the card
    /// drops the column entirely ([`Breakdown::is_priced`]), so that state is
    /// not worded here.
    pub fn price_label(&self) -> String {
        if self.is_unpriced() {
            return UNPRICED.to_string();
        }
        match self.cost {
            Some(cost) if !self.usage.is_zero() => human_usd(cost),
            _ => NO_FIGURE.to_string(),
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
    ///
    /// Zero for an unpriced row under [`Ranking::Spend`], and deliberately so:
    /// the track is drawn against money, and a bar scaled to *this* row's
    /// tokens beside bars scaled to everyone else's dollars would be one
    /// picture of two quantities. What the row does know is in the token
    /// column, and [`BreakdownRow::price_label`] says why the track is empty.
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
/// **A tie on the ranking quantity breaks on tokens before it breaks on the
/// alphabet** (gh#359). That rule exists for one row in particular: a model the
/// rate table has never heard of spends nothing measurable, so under
/// [`Ranking::Spend`] every unpriced row ties at zero — and ordering the rows
/// the card *cannot* price by the alphabet throws away the one number the board
/// does know exactly about them. Ranked by tokens they sit under the priced
/// rows, biggest first, which is the reading a person opening a usage view
/// wants: the unpriced model doing the most work is the top of that group.
/// Alphabetical remains the last word, so the order is still stable.
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
            .then_with(|| b.usage.total().cmp(&a.usage.total()))
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
    /// How many attempts exposed per-agent detail (gh#426). Read beside
    /// [`attempts_with_tokens`](Self::attempts_with_tokens): a harness can
    /// report an exact total without exposing who inside the run spent it.
    #[serde(default)]
    pub attempts_with_agent_usage: usize,
    /// Main/subagent/model rows over the attempts that exposed them. Absent on
    /// older boxes and empty rather than zero-valued when nothing attributed.
    #[serde(default)]
    pub agent_usage: Vec<AgentSpend>,

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

    /// Its list-price API estimate, beside what the plans behind it cost
    /// (gh#182).
    ///
    /// `None` is **rates not configured** — said out loud rather than rendered
    /// as a confident `$0.00`, which is gh#96's lesson applied to money. A
    /// board that was given rates and simply spent nothing carries a `Some`
    /// whose total is zero, and those two are different facts.
    #[serde(default)]
    pub spend: Option<BoardSpend>,

    /// Applies to every dollar-valued `listPrice` / `cost` field in this
    /// response. Serde-defaulted so a newer viewport can still read an older
    /// box, while every newly emitted JSON response labels the figures.
    #[serde(default)]
    pub pricing_basis: PricingBasis,

    /// How close this window's attempts came to filling their context windows
    /// (gh#271). Defaulted on the wire, like every field added after the
    /// first release: an older board answers without it and the page simply
    /// does not draw the line.
    #[serde(default)]
    pub context: ContextPressure,

    /// Any attempt on record at all — all time, never windowed (gh#434).
    ///
    /// The same evidence [`crate::view::board::board_dispatched`] reads off
    /// the rows: a board somebody has released work from is the org's board,
    /// and one that only ever collected rows is furniture. A host sweep holds
    /// a furniture answer as a fallback and keeps asking rather than settling
    /// on it — which is why this cannot be read off
    /// [`attempts`](Self::attempts): the box's board on a quiet week is not
    /// furniture, and a windowed count would say it was.
    ///
    /// Defaulted on the wire: a board that predates this field answers
    /// without it and reads as furniture, which costs it only the tie
    /// against a board that says otherwise.
    #[serde(default)]
    pub dispatched: bool,
}

// -- all boards (gh#461) ----------------------------------------------------

/// The two facts that a finished [`BoardStats`] has intentionally compressed,
/// but an all-board answer needs in order to compress the union correctly.
///
/// Percentiles cannot be averaged, and the per-dimension rows in
/// [`BoardStats::breakdown`] have already folded their tail. A board therefore
/// sends these two small, bounded-by-history inputs only to the on-demand
/// aggregate collector. They are not stored, streamed, or rendered.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StatsMergeBasis {
    /// Every ended attempt's duration, already window-filtered and sorted.
    pub duration_minutes: Vec<i64>,
    /// The same five cuts as [`BoardStats::breakdown`], before its display cap.
    pub breakdown: Vec<Breakdown>,
    /// The durable identity behind each display account label. Configured
    /// payer ids may be shared by boards; an unnamed login belongs only to
    /// the board that reported it and is qualified by `board_id` when merged.
    pub account_identities: BTreeMap<String, StatsAccountIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "scope")]
pub enum StatsAccountIdentity {
    Shared { account_id: String },
    BoardLocal,
}

/// A device as the aggregate contract names it.
///
/// Kept separate from `Device`: a stats answer needs only the durable id and
/// the human label, and carrying presence/platform fields into an audit record
/// would make them look like facts about when the stats were produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsDevice {
    pub device_id: String,
    pub label: String,
}

/// One board's answer before duplicate transport paths are collapsed.
///
/// `board_id` belongs to `board.db`, not to the device. Copying or aliasing the
/// same store therefore keeps one identity, while two boards polling the same
/// repository remain two identities and keep both sets of attempts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardStatsSnapshot {
    pub board_id: String,
    /// The engine that actually read the store. This may differ from the
    /// candidate path used to reach it, which is recorded in [`StatsHost`].
    pub host: StatsDevice,
    pub stats: BoardStats,
    #[serde(default)]
    pub merge_basis: StatsMergeBasis,
}

/// One board after transport aliases have been collapsed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AggregateBoardStatsSource {
    pub board_id: String,
    pub host: StatsDevice,
    pub stats: BoardStats,
}

/// What happened when the collector asked one device candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StatsHostStatus {
    /// This path contributed a board.
    Answered,
    /// This path answered the same canonical board as an earlier path.
    Duplicate,
    /// The engine answered and explicitly hosts no board.
    NoBoard,
    /// Nobody answered within the collection budget, or the transport failed.
    Unreachable,
    /// A host answered, but not with a snapshot this collector could read.
    Unreadable,
    /// The host is reachable, but its engine predates the snapshot protocol.
    /// It contributed no totals until the operator deliberately updates it.
    UpgradeRequired,
}

impl StatsHostStatus {
    pub fn compromises_aggregate(self) -> bool {
        matches!(
            self,
            Self::Unreachable | Self::Unreadable | Self::UpgradeRequired
        )
    }
}

/// The audit row for one candidate transport path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsHost {
    pub device: StatsDevice,
    pub status: StatsHostStatus,
    /// Present on both `answered` and `duplicate`, linking aliases to the
    /// single entry in [`AggregateBoardStats::boards`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board_id: Option<String>,
    /// Human-readable transport/schema failure. Never used for `noBoard`:
    /// hosting no board is a complete answer, not a failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Present exactly for an `upgradeRequired` host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade: Option<StatsUpgradeDetails>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsUpgradeDetails {
    pub current_version: String,
    pub required_version: String,
    /// The rejected snapshot call, retained separately from the diagnosis.
    pub error: String,
    /// The peer itself reported over the compatible `UpdateStatus` stream
    /// that `ApplyUpdate` would accept (`canApply`, gh#486) — the running
    /// process's own `detect_install` answer, so it does not depend on the
    /// peer's possibly stale release-check cache. Peers that predate the
    /// field prove nothing and stay `false`: a stale managed tree beside a
    /// source build is not the running executable, so no button is offered.
    #[serde(default)]
    pub can_apply: bool,
}

/// The one aggregate contract shared by CLI JSON, desktop and iOS.
///
/// `stats` has exactly the single-board shape renderers already understand.
/// `boards` preserves the individual answers and their canonical ownership;
/// `hosts` says how complete the fan-out was. An empty `stats` value is never
/// evidence of zero activity unless `complete` is true.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AggregateBoardStats {
    pub since_days: Option<i64>,
    pub stats: BoardStats,
    pub boards: Vec<AggregateBoardStatsSource>,
    pub hosts: Vec<StatsHost>,
    pub complete: bool,
}

impl AggregateBoardStats {
    /// The warning every surface says over a partial union.
    pub fn completeness_note(&self) -> Option<String> {
        let missing: Vec<String> = self
            .hosts
            .iter()
            .filter(|host| host.status.compromises_aggregate())
            .map(|host| match host.status {
                StatsHostStatus::UpgradeRequired => format!(
                    "{} is on v{}; v{} is required for all-board stats",
                    host.device.label,
                    host.upgrade
                        .as_ref()
                        .map(|u| u.current_version.as_str())
                        .unwrap_or("unknown"),
                    host.upgrade
                        .as_ref()
                        .map(|u| u.required_version.as_str())
                        .unwrap_or("unknown")
                ),
                StatsHostStatus::Unreadable => format!("{} was unreadable", host.device.label),
                _ => format!("{} did not answer", host.device.label),
            })
            .collect();
        (!missing.is_empty()).then(|| {
            format!(
                "Partial aggregate — {}. The totals include only the boards that answered.",
                missing.join("; ")
            )
        })
    }
}

/// One candidate's result, before [`aggregate_board_stats`] turns transport
/// outcomes into the stable wire contract. This is the collector's test seam:
/// production supplies relay answers; tests supply values directly.
#[derive(Debug, Clone, PartialEq)]
pub struct StatsProbe {
    pub candidate: StatsDevice,
    pub result: StatsProbeResult,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StatsProbeResult {
    Answered(BoardStatsSnapshot),
    NoBoard,
    Unreachable(String),
    Unreadable(String),
    UpgradeRequired {
        current_version: String,
        required_version: String,
        error: String,
        can_apply: bool,
    },
}

/// The context half of the page: not what the window's work cost, but how
/// close it ran to the limit of what its agents could hold (gh#271).
///
/// Deliberately three small numbers rather than a distribution. The question
/// this answers is whether attempts on this board are routinely running out of
/// context — which is a fact about how the work is *shaped* (too much in one
/// attempt) and shows up nowhere in the spend, because a compacting agent and
/// a comfortable one cost about the same.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ContextPressure {
    /// Attempts in the window whose harness reported a window at all — this
    /// half of the page's coverage line, and the same honesty
    /// [`BoardStats::attempts_with_tokens`] keeps: the two below are shares of
    /// this, never of the window.
    pub attempts_reported: usize,
    /// …of which this many were last seen at or past the point their harness
    /// compacts (or, for a harness that names no point, 90% of the window).
    pub near_compaction: usize,
    /// The fullest any one attempt was last seen, `0..=100`. `None` when
    /// nothing reported — never `Some(0)`, which would read as a board whose
    /// agents ran empty.
    pub peak_percent: Option<u8>,
}

impl ContextPressure {
    /// Did anything in this window report a context window at all? The gate
    /// the line renders behind — with nothing reported there is no share to
    /// show, and a `0 of 0` would read as a board with no pressure rather than
    /// one with no measurements.
    pub fn is_reported(&self) -> bool {
        self.attempts_reported > 0
    }
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
            attempts_with_agent_usage: 0,
            agent_usage: Vec::new(),
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
            pricing_basis: PricingBasis::ListPriceApiEstimate,
            context: ContextPressure::default(),
            dispatched: false,
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

/// Collapse a bounded set of host probes into one auditable union.
///
/// Boards are keyed only by their store identity. Repository/task ids are not
/// deduplication keys: two stores may legitimately poll the same repository,
/// and the distinct attempts they released are precisely what this view must
/// reveal. Probe order is retained for host audit rows; contributing boards
/// retain their first candidate's order. The collector supplies local-first,
/// then registration order, and `join_all` preserves it despite concurrent
/// completion — deterministic JSON without changing the per-board selector's
/// established tie-break.
pub fn aggregate_board_stats(
    since_days: Option<i64>,
    probes: Vec<StatsProbe>,
) -> AggregateBoardStats {
    let mut hosts = Vec::with_capacity(probes.len());
    let mut snapshots: Vec<BoardStatsSnapshot> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    for probe in probes {
        let candidate = probe.candidate;
        match probe.result {
            StatsProbeResult::Answered(snapshot) if snapshot.board_id.trim().is_empty() => {
                hosts.push(StatsHost {
                    device: candidate,
                    status: StatsHostStatus::Unreadable,
                    board_id: None,
                    error: Some("the board answered without a stable board id".into()),
                    upgrade: None,
                });
            }
            StatsProbeResult::Answered(snapshot) => {
                let board_id = snapshot.board_id.clone();
                let duplicate = !seen.insert(board_id.clone());
                if !duplicate {
                    snapshots.push(snapshot);
                }
                hosts.push(StatsHost {
                    device: candidate,
                    status: if duplicate {
                        StatsHostStatus::Duplicate
                    } else {
                        StatsHostStatus::Answered
                    },
                    board_id: Some(board_id),
                    error: None,
                    upgrade: None,
                });
            }
            StatsProbeResult::NoBoard => hosts.push(StatsHost {
                device: candidate,
                status: StatsHostStatus::NoBoard,
                board_id: None,
                error: None,
                upgrade: None,
            }),
            StatsProbeResult::Unreachable(error) => hosts.push(StatsHost {
                device: candidate,
                status: StatsHostStatus::Unreachable,
                board_id: None,
                error: Some(error),
                upgrade: None,
            }),
            StatsProbeResult::Unreadable(error) => hosts.push(StatsHost {
                device: candidate,
                status: StatsHostStatus::Unreadable,
                board_id: None,
                error: Some(error),
                upgrade: None,
            }),
            StatsProbeResult::UpgradeRequired {
                current_version,
                required_version,
                error,
                can_apply,
            } => hosts.push(StatsHost {
                device: candidate,
                status: StatsHostStatus::UpgradeRequired,
                board_id: None,
                error: None,
                upgrade: Some(StatsUpgradeDetails {
                    current_version,
                    required_version,
                    error,
                    can_apply,
                }),
            }),
        }
    }

    let stats = merge_board_stats(since_days, &snapshots);
    let boards = snapshots
        .iter()
        .map(|snapshot| AggregateBoardStatsSource {
            board_id: snapshot.board_id.clone(),
            host: snapshot.host.clone(),
            stats: snapshot.stats.clone(),
        })
        .collect();
    let complete = !hosts.iter().any(|host| host.status.compromises_aggregate());

    AggregateBoardStats {
        since_days,
        stats,
        boards,
        hosts,
        complete,
    }
}

fn add_counts(into: &mut BTreeMap<String, usize>, from: &BTreeMap<String, usize>) {
    for (key, count) in from {
        *into.entry(key.clone()).or_default() += count;
    }
}

fn add_tokens(into: &mut BTreeMap<String, TokenUsage>, from: &BTreeMap<String, TokenUsage>) {
    for (key, usage) in from {
        into.entry(key.clone()).or_default().add(*usage);
    }
}

fn nearest_rank(sorted: &[i64], p: f64) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((sorted.len() as f64) * p).ceil() as usize;
    sorted.get(rank.saturating_sub(1)).copied()
}

#[derive(Default)]
struct AgentSpendMerge {
    agent: Option<AgentKind>,
    name: Option<String>,
    model: String,
    usage: TokenUsage,
    priced: bool,
    list_price: Usd,
    unpriced_tokens: u64,
}

fn merge_agent_usage(snapshots: &[BoardStatsSnapshot]) -> Vec<AgentSpend> {
    let mut rows: Vec<AgentSpendMerge> = Vec::new();
    for row in snapshots
        .iter()
        .flat_map(|snapshot| &snapshot.stats.agent_usage)
    {
        let found = rows.iter_mut().find(|found| {
            found.agent == Some(row.agent) && found.name == row.name && found.model == row.model
        });
        let found = match found {
            Some(found) => found,
            None => {
                rows.push(AgentSpendMerge {
                    agent: Some(row.agent),
                    name: row.name.clone(),
                    model: row.model.clone(),
                    ..Default::default()
                });
                rows.last_mut().expect("just pushed")
            }
        };
        found.usage.add(row.usage);
        match row.list_price_api_estimate {
            Some(cost) => {
                found.priced = true;
                found.list_price += cost;
                found.unpriced_tokens += row.unpriced_tokens;
            }
            None => {
                // A board with no rates is not free. If another board did
                // price this row, its known dollars remain visible and this
                // board's whole slice is carried beside them as unpriced.
                found.unpriced_tokens += row.usage.total();
            }
        }
    }
    let mut rows: Vec<AgentSpend> = rows
        .into_iter()
        .map(|row| AgentSpend {
            agent: row.agent.expect("every row came from an agent"),
            name: row.name,
            model: row.model,
            usage: row.usage,
            list_price_api_estimate: row.priced.then_some(row.list_price),
            unpriced_tokens: row.unpriced_tokens,
        })
        .collect();
    rows.sort_by(|a, b| {
        b.list_price_api_estimate
            .cmp(&a.list_price_api_estimate)
            .then_with(|| b.usage.total().cmp(&a.usage.total()))
            .then_with(|| a.label().cmp(&b.label()))
    });
    rows
}

#[derive(Default)]
struct BreakdownMerge {
    dispatches: usize,
    usage: TokenUsage,
    cost: Usd,
    saw_price: bool,
    unpriced_tokens: u64,
}

fn aggregate_account_label(snapshot: &BoardStatsSnapshot, label: &str) -> String {
    match snapshot.merge_basis.account_identities.get(label) {
        Some(StatsAccountIdentity::Shared { account_id }) => account_id.clone(),
        Some(StatsAccountIdentity::BoardLocal) | None => {
            format!("{label} ({})", snapshot.board_id)
        }
    }
}

fn merge_breakdown(snapshots: &[BoardStatsSnapshot]) -> Vec<Breakdown> {
    let mut cuts: BTreeMap<(Dimension, String), BreakdownMerge> = BTreeMap::new();
    for snapshot in snapshots {
        let source = if snapshot.merge_basis.breakdown.is_empty() {
            &snapshot.stats.breakdown
        } else {
            &snapshot.merge_basis.breakdown
        };
        for cut in source {
            for row in &cut.rows {
                let label = if cut.dimension == Dimension::Account {
                    aggregate_account_label(snapshot, &row.label)
                } else {
                    row.label.clone()
                };
                let merged = cuts.entry((cut.dimension, label)).or_default();
                merged.dispatches += row.dispatches;
                merged.usage.add(row.usage);
                match row.cost {
                    Some(cost) => {
                        merged.saw_price = true;
                        merged.cost += cost;
                        merged.unpriced_tokens += row.unpriced_tokens;
                    }
                    None => merged.unpriced_tokens += row.usage.total(),
                }
            }
        }
    }
    Dimension::ALL
        .iter()
        .filter_map(|dimension| {
            let rows: Vec<BreakdownRow> = cuts
                .iter()
                .filter(|((candidate, _), _)| candidate == dimension)
                .map(|((_, label), row)| BreakdownRow {
                    label: label.clone(),
                    dispatches: row.dispatches,
                    usage: row.usage,
                    cost: row.saw_price.then_some(row.cost),
                    unpriced_tokens: row.unpriced_tokens,
                })
                .collect();
            (!rows.is_empty()).then(|| rank_breakdown(*dimension, rows, BREAKDOWN_ROWS))
        })
        .collect()
}

fn merge_rate_tables(spends: &[&BoardSpend]) -> RateTable {
    let mut tables = spends.iter().map(|spend| &spend.rates);
    let Some(first) = tables.next() else {
        return RateTable::empty("");
    };
    let mut merged = first.clone();
    let mut conflicted = std::collections::BTreeSet::new();
    for table in tables {
        if !table.as_of.is_empty() && (merged.as_of.is_empty() || table.as_of < merged.as_of) {
            merged.as_of = table.as_of.clone();
        }
        for (key, rate) in &table.entries {
            match merged.entries.get(key) {
                Some(existing) if existing != rate => {
                    merged.entries.remove(key);
                    conflicted.insert(key.clone());
                }
                None if !conflicted.contains(key) => {
                    merged.entries.insert(key.clone(), *rate);
                }
                _ => {}
            }
        }
        merged.overridden.extend(table.overridden.iter().cloned());
    }
    merged.overridden.sort();
    merged.overridden.dedup();
    merged
        .overridden
        .retain(|key| merged.entries.contains_key(key));
    merged
}

#[derive(Default)]
struct AccountSpendMerge {
    attempts: usize,
    usage: TokenUsage,
    list_price: Usd,
    unpriced_tokens: u64,
    plan: Option<AccountPlan>,
    plan_in_window: Option<Usd>,
    plan_conflict: bool,
}

fn add_account_row(accounts: &mut BTreeMap<String, AccountSpendMerge>, row: AccountSpend) {
    let merged = accounts.entry(row.label).or_default();
    merged.attempts += row.attempts;
    merged.usage.add(row.usage);
    merged.list_price += row.list_price;
    merged.unpriced_tokens += row.unpriced_tokens;
    if let Some(plan) = row.plan {
        match &merged.plan {
            None if !merged.plan_conflict => {
                merged.plan = Some(plan);
                merged.plan_in_window = row.plan_in_window;
            }
            Some(existing) if existing == &plan => {
                if merged.plan_in_window.is_none() {
                    merged.plan_in_window = row.plan_in_window;
                }
            }
            _ => {
                // Two different declarations for one subscription cannot be
                // added or arbitrarily preferred. The per-board entries keep
                // both for audit; the union leaves the plan unconfigured.
                merged.plan = None;
                merged.plan_in_window = None;
                merged.plan_conflict = true;
            }
        }
    }
}

fn merge_spend(snapshots: &[BoardStatsSnapshot]) -> Option<BoardSpend> {
    let spends: Vec<&BoardSpend> = snapshots
        .iter()
        .filter_map(|snapshot| snapshot.stats.spend.as_ref())
        .collect();
    if spends.is_empty() {
        return None;
    }

    let mut by_model: Vec<ModelSpend> = Vec::new();
    let mut unpriced: BTreeMap<String, TokenUsage> = BTreeMap::new();
    let mut accounts: BTreeMap<String, AccountSpendMerge> = BTreeMap::new();
    let mut list_price = Usd::ZERO;

    for snapshot in snapshots {
        match &snapshot.stats.spend {
            Some(spend) => {
                list_price += spend.list_price;
                for row in &spend.by_model {
                    if let Some(existing) = by_model.iter_mut().find(|existing| {
                        existing.label == row.label
                            && existing.rate_key == row.rate_key
                            && existing.source == row.source
                            && existing.rate == row.rate
                    }) {
                        existing.usage.add(row.usage);
                        existing.cost += row.cost;
                    } else {
                        by_model.push(row.clone());
                    }
                }
                for row in &spend.unpriced {
                    unpriced
                        .entry(row.label.clone())
                        .or_default()
                        .add(row.usage);
                }
                for row in &spend.accounts {
                    let mut row = row.clone();
                    row.label = aggregate_account_label(snapshot, &row.label);
                    add_account_row(&mut accounts, row);
                }
            }
            None => {
                // Rates absent on this board: every metered model is explicitly
                // outside the known total once any other board supplies rates.
                for (model, usage) in &snapshot.stats.tokens_by_model {
                    unpriced.entry(model.clone()).or_default().add(*usage);
                }
                for (label, attempts) in &snapshot.stats.by_account {
                    let usage = snapshot
                        .stats
                        .tokens_by_account
                        .get(label)
                        .copied()
                        .unwrap_or_default();
                    add_account_row(
                        &mut accounts,
                        AccountSpend {
                            label: aggregate_account_label(snapshot, label),
                            attempts: *attempts,
                            usage,
                            list_price: Usd::ZERO,
                            unpriced_tokens: usage.total(),
                            plan: None,
                            plan_in_window: None,
                        },
                    );
                }
            }
        }
    }

    by_model.sort_by(|a, b| b.cost.cmp(&a.cost).then_with(|| a.label.cmp(&b.label)));
    let mut unpriced: Vec<TokenTally> = unpriced
        .into_iter()
        .filter(|(_, usage)| !usage.is_zero())
        .map(|(label, usage)| TokenTally { label, usage })
        .collect();
    unpriced.sort_by(|a, b| {
        b.usage
            .total()
            .cmp(&a.usage.total())
            .then_with(|| a.label.cmp(&b.label))
    });
    let unpriced_tokens = unpriced.iter().map(|row| row.usage.total()).sum();
    let mut accounts: Vec<AccountSpend> = accounts
        .into_iter()
        .map(|(label, row)| AccountSpend {
            label,
            attempts: row.attempts,
            usage: row.usage,
            list_price: row.list_price,
            unpriced_tokens: row.unpriced_tokens,
            plan: row.plan,
            plan_in_window: row.plan_in_window,
        })
        .collect();
    accounts.sort_by(|a, b| {
        b.list_price
            .cmp(&a.list_price)
            .then_with(|| a.label.cmp(&b.label))
    });

    Some(BoardSpend {
        rates: merge_rate_tables(&spends),
        list_price,
        by_model,
        unpriced,
        unpriced_tokens,
        accounts,
    })
}

fn merge_board_stats(since_days: Option<i64>, snapshots: &[BoardStatsSnapshot]) -> BoardStats {
    let mut merged = BoardStats::empty(since_days);
    let mut durations = Vec::new();
    let mut days: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut token_days: BTreeMap<String, TokenUsage> = BTreeMap::new();

    for snapshot in snapshots {
        let stats = &snapshot.stats;
        merged.attempts += stats.attempts;
        merged.tasks_touched += stats.tasks_touched;
        add_counts(&mut merged.outcomes, &stats.outcomes);
        merged.live += stats.live;
        merged.total_minutes += stats.total_minutes;
        merged.tokens.add(stats.tokens);
        merged.attempts_with_tokens += stats.attempts_with_tokens;
        merged.attempts_with_agent_usage += stats.attempts_with_agent_usage;
        merged.landing.merged += stats.landing.merged;
        merged.landing.open += stats.landing.open;
        merged.landing.closed_unmerged += stats.landing.closed_unmerged;
        merged.landing.no_pr += stats.landing.no_pr;
        merged.landing.in_flight += stats.landing.in_flight;
        merged.friction.retried_tasks += stats.friction.retried_tasks;
        merged.friction.early_settles += stats.friction.early_settles;
        merged.friction.blocked_entries += stats.friction.blocked_entries;
        merged.friction.overruns += stats.friction.overruns;
        merged.agent_dispatched += stats.agent_dispatched;
        merged.context.attempts_reported += stats.context.attempts_reported;
        merged.context.near_compaction += stats.context.near_compaction;
        merged.context.peak_percent =
            match (merged.context.peak_percent, stats.context.peak_percent) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
        merged.dispatched |= stats.dispatched;

        for day in &stats.daily {
            let entry = days.entry(day.date.clone()).or_default();
            entry.0 += day.dispatches;
            entry.1 += day.done;
        }
        for day in &stats.daily_tokens {
            token_days
                .entry(day.date.clone())
                .or_default()
                .add(day.usage);
        }
        for (slot, count) in merged.hour_of_day.iter_mut().zip(&stats.hour_of_day) {
            *slot += count;
        }
        for (workspace, hours) in &stats.hours_by_workspace {
            let row = merged
                .hours_by_workspace
                .entry(workspace.clone())
                .or_insert_with(|| vec![0; HOURS]);
            for (slot, count) in row.iter_mut().zip(hours) {
                *slot += count;
            }
        }
        add_counts(&mut merged.by_workspace, &stats.by_workspace);
        add_counts(&mut merged.by_runtime, &stats.by_runtime);
        add_counts(&mut merged.by_source, &stats.by_source);
        for (label, count) in &stats.by_account {
            *merged
                .by_account
                .entry(aggregate_account_label(snapshot, label))
                .or_default() += count;
        }
        add_tokens(&mut merged.tokens_by_model, &stats.tokens_by_model);
        add_tokens(&mut merged.tokens_by_runtime, &stats.tokens_by_runtime);
        for (label, usage) in &stats.tokens_by_account {
            merged
                .tokens_by_account
                .entry(aggregate_account_label(snapshot, label))
                .or_default()
                .add(*usage);
        }
        durations.extend(snapshot.merge_basis.duration_minutes.iter().copied());
    }

    durations.sort_unstable();
    let ended: usize = merged.outcomes.values().sum();
    let done = merged.outcomes.get("done").copied().unwrap_or(0);
    merged.completion_rate = (ended > 0).then(|| done as f64 / ended as f64);
    merged.token_coverage =
        (merged.attempts > 0).then(|| merged.attempts_with_tokens as f64 / merged.attempts as f64);
    merged.median_minutes = nearest_rank(&durations, 0.5);
    merged.p90_minutes = nearest_rank(&durations, 0.9);
    merged.longest_minutes = durations.last().copied();
    merged.agent_usage = merge_agent_usage(snapshots);
    merged.breakdown = merge_breakdown(snapshots);
    merged.spend = merge_spend(snapshots);
    merged.daily = days
        .into_iter()
        .map(|(date, (dispatches, done))| DayBucket {
            date,
            dispatches,
            done,
        })
        .collect();
    merged.daily_tokens = merged
        .daily
        .iter()
        .map(|day| TokenDay {
            date: day.date.clone(),
            usage: token_days.get(&day.date).copied().unwrap_or_default(),
        })
        .collect();
    merged
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

// ---------------------------------------------------------------------------
// What a mark says when asked (gh#469)
// ---------------------------------------------------------------------------

/// What one chart mark says when asked (gh#469): the bucket or category the
/// mark stands for, and one line per figure it carries, each with its unit and
/// series name.
///
/// One derivation for every surface that reads a mark — the desktop tooltip,
/// its keyboard detail, and both platforms' accessibility labels — because a
/// hover that says `1.31M` beside a screen reader that says something else is
/// two answers to one question. Figures keep the page's own formats
/// ([`human_tokens`], [`human_usd`]) with the exact count beside the rounded
/// one, so a reader stops estimating from geometry without the detail
/// inventing a fourth number format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkDetail {
    /// The bucket or category: `Tue 5 Aug 2026`, `comet-native · 21:00–22:00`,
    /// `output`.
    pub title: String,
    /// The figures, one per series the mark carries: `540 tokens`,
    /// `8 dispatches`, `$0.40 at list price`.
    pub lines: Vec<String>,
}

impl MarkDetail {
    /// The whole detail as one sentence — what assistive technology announces
    /// for the mark.
    pub fn sentence(&self) -> String {
        format!("{}: {}", self.title, self.lines.join(", "))
    }
}

/// An exact count a detail can quote: `1,310,442`. The page's figures stay
/// [`human_tokens`] — this appears beside them, never instead of them.
pub fn group_digits(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// A mark's token figure: the page's own rounding with the exact count beside
/// it once they differ — `1.31M tokens (1,310,442)`, `812 tokens`.
pub fn tokens_line(tokens: u64) -> String {
    let human = human_tokens(tokens);
    let exact = group_digits(tokens);
    if human == exact {
        format!("{human} tokens")
    } else {
        format!("{human} tokens ({exact})")
    }
}

/// A mark's dispatch figure: `no dispatches`, `1 dispatch`, `7 dispatches` — a
/// zero says the bucket is empty rather than printing a figure nobody needs.
pub fn dispatches_line(count: usize) -> String {
    match count {
        0 => "no dispatches".to_string(),
        1 => "1 dispatch".to_string(),
        n => format!("{n} dispatches"),
    }
}

/// A mark's day as its detail says it: `Tue 5 Aug 2026`. Longer than the
/// caption's `Tue 5`, because a detail is read alone, without the chart's
/// window under it. A date that will not parse is returned as it came.
pub fn mark_day(date: &str) -> String {
    match chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d") {
        Ok(day) => day.format("%a %-d %b %Y").to_string(),
        Err(_) => date.to_string(),
    }
}

/// The hour bucket `14` as a detail says it: `14:00–15:00`, wrapping at the
/// end of the day so 23 reads `23:00–00:00`.
pub fn hour_span(hour: usize) -> String {
    let hour = hour % HOURS;
    format!("{:02}:00–{:02}:00", hour, (hour + 1) % HOURS)
}

impl DayColumn {
    /// What this column says when asked (gh#469): the day, what it spent, and
    /// what ran — both series, because both are in the mark.
    pub fn detail(&self) -> MarkDetail {
        MarkDetail {
            title: mark_day(&self.date),
            lines: vec![
                if self.tokens == 0 {
                    "no tokens reported".to_string()
                } else {
                    tokens_line(self.tokens)
                },
                dispatches_line(self.dispatches),
            ],
        }
    }
}

/// The dispatch chart's mark (the phone still plots dispatches): the day, what
/// ran, and how much of it has since ended `done` — the stacked series named
/// apart, so a reader can tell the solid band from the whole bar.
pub fn day_bucket_detail(day: &DayBucket) -> MarkDetail {
    let lines = if day.dispatches == 0 {
        vec!["no dispatches".to_string()]
    } else {
        vec![
            dispatches_line(day.dispatches),
            format!("{} ended done", day.done),
        ]
    };
    MarkDetail {
        title: mark_day(&day.date),
        lines,
    }
}

/// One cell of the crossing — or one bar of the degraded hour histogram, whose
/// space is `every space`: which space, which hour, and what was released then.
pub fn hour_cell_detail(space: &str, hour: usize, count: usize) -> MarkDetail {
    MarkDetail {
        title: format!("{space} · {}", hour_span(hour)),
        lines: vec![dispatches_line(count)],
    }
}

impl CostSlice {
    /// gh#469: the token class, what it cost, what that bought, and its share
    /// of the priced total.
    pub fn detail(&self) -> MarkDetail {
        let mut lines = vec![
            format!("{} at list price", human_usd(self.cost)),
            tokens_line(self.tokens),
        ];
        if let Some(share) = percent(Some(self.share)) {
            lines.push(format!("{share} of the priced total"));
        }
        MarkDetail {
            title: self.label().to_string(),
            lines,
        }
    }
}

impl LandingSegment {
    /// gh#469: the outcome, the count, and its share of what landed. An empty
    /// category says `no tasks` and claims no share — a percentage of nothing
    /// landed is not a figure.
    pub fn detail(&self) -> MarkDetail {
        let mut lines = vec![match self.count {
            0 => "no tasks".to_string(),
            1 => "1 task".to_string(),
            n => format!("{n} tasks"),
        }];
        if self.count > 0 {
            if let Some(share) = percent(Some(self.fraction)) {
                lines.push(format!("{share} of what landed"));
            }
        }
        MarkDetail {
            title: self.label.clone(),
            lines,
        }
    }
}

impl BreakdownRow {
    /// gh#469: what the row's track says when asked — the row's own three
    /// facts plus the dispatch count the columns have no room for, and the
    /// same three money states as [`BreakdownRow::price_label`], worded so a
    /// missing figure is a reason rather than a blank.
    pub fn detail(&self, dimension: Dimension) -> MarkDetail {
        let mut lines = vec![dispatches_line(self.dispatches)];
        if self.usage.is_zero() {
            lines.push("no token usage reported".to_string());
        } else {
            lines.push(tokens_line(self.usage.total()));
        }
        if self.is_unpriced() {
            lines.push("unpriced — no rate for this model".to_string());
        } else if let Some(cost) = self.cost.filter(|_| !self.usage.is_zero()) {
            lines.push(format!("{} at list price", human_usd(cost)));
            if self.unpriced_tokens > 0 {
                lines.push(format!(
                    "{} of that unpriced",
                    human_tokens(self.unpriced_tokens)
                ));
            }
        }
        MarkDetail {
            title: format!("{} · {}", self.label, dimension.label()),
            lines,
        }
    }
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

// ---------------------------------------------------------------------------
// Which board the page is reading (gh#254)
// ---------------------------------------------------------------------------

/// One board a stats sweep found, and enough of what it holds to say whether
/// it is the one worth reading.
///
/// A viewport sweeping [`crate::view::board::host_candidates`] settles on
/// whichever device answers first, and on a laptop running Comet beside the box
/// that is the laptop — §gh#195's "two boards and neither knew", arrived at by
/// a page rather than by a doctor. The sweep can see the other candidates, so
/// the page it feeds can name them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostBoard {
    /// The device hosting it. `None` is this device — the same absent
    /// `targetDeviceId` the sweep uses, so the two representations match.
    pub device_id: Option<String>,
    /// What to call it: the device's name.
    pub label: String,
    pub attempts: usize,
    pub attempts_with_tokens: usize,
    pub tokens: u64,
}

impl HostBoard {
    /// Read one off a window's answer.
    pub fn of(device_id: Option<String>, label: impl Into<String>, stats: &BoardStats) -> Self {
        Self {
            device_id,
            label: label.into(),
            attempts: stats.attempts,
            attempts_with_tokens: stats.attempts_with_tokens,
            tokens: stats.tokens.total(),
        }
    }

    /// Did anything on it report token usage? The question the spend card is
    /// really asking, and the one that separates a board worth opening from a
    /// board that will read "nothing metered" forever.
    pub fn has_tokens(&self) -> bool {
        self.attempts_with_tokens > 0
    }

    /// Why this board has no money on it, in its own terms.
    ///
    /// The three cases are three different facts and a page that collapsed them
    /// would be back where this ticket started: nothing was ever dispatched
    /// here, work ran here but none of it was metered, or it was metered and
    /// the rates could not price it (which is not this function's business —
    /// [`BoardStats::spend_label`] owns that one).
    pub fn emptiness(&self) -> String {
        match (self.attempts, self.attempts_with_tokens) {
            (0, _) => format!(
                "Nothing has been dispatched from the board on {}.",
                self.label
            ),
            (attempts, 0) => format!(
                "The board on {} has {}, none of which recorded token usage.",
                self.label,
                attempts_phrase(attempts)
            ),
            (attempts, metered) => format!(
                "The board on {} recorded {} over {} of {}.",
                self.label,
                human_tokens(self.tokens),
                metered,
                attempts_phrase(attempts)
            ),
        }
    }
}

/// **What the sweep found that the page is not showing (gh#254).**
///
/// `nothing metered to price` is true and useless when the sweep resolved onto
/// an empty board and a full one was one candidate further down the list. This
/// is the sentence that names both: what THIS board holds, and what the others
/// hold — the reader can then disbelieve the empty state on evidence rather
/// than on suspicion.
///
/// `None` when the sweep found only one board: there is nothing to compare
/// against, and "no other board exists" is not news to somebody running one.
pub fn other_boards_note(boards: &[HostBoard], current: Option<&str>) -> Option<String> {
    let here = boards.iter().find(|b| b.device_id.as_deref() == current)?;
    let elsewhere = elsewhere_note(boards, current)?;
    Some(format!("{} {elsewhere}", here.emptiness()))
}

/// The second half alone: what the *other* boards hold.
///
/// The page's own empty state already says nothing was dispatched in the
/// window, so it wants this half without [`HostBoard::emptiness`] repeating it
/// back in different words.
pub fn elsewhere_note(boards: &[HostBoard], current: Option<&str>) -> Option<String> {
    // A current host nothing answered for has no board to write about, and
    // naming the others under it would imply a comparison there is no left
    // side to.
    boards.iter().find(|b| b.device_id.as_deref() == current)?;
    let others: Vec<&HostBoard> = boards
        .iter()
        .filter(|b| b.device_id.as_deref() != current)
        .collect();
    if others.is_empty() {
        return None;
    }

    // Biggest first: if one of the others is the board the work actually runs
    // on, it is the one the reader wants named first.
    let mut with_tokens: Vec<&HostBoard> =
        others.iter().copied().filter(|b| b.has_tokens()).collect();
    with_tokens.sort_by(|a, b| b.tokens.cmp(&a.tokens).then_with(|| a.label.cmp(&b.label)));

    Some(if with_tokens.is_empty() {
        format!(
            "The other {} the sweep found ({}) recorded no token usage either.",
            if others.len() == 1 { "board" } else { "boards" },
            others
                .iter()
                .map(|b| b.label.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else {
        let listed = with_tokens
            .iter()
            .map(|b| {
                format!(
                    "the board on {} has {} tokens over {}",
                    b.label,
                    human_tokens(b.tokens),
                    attempts_phrase(b.attempts)
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        format!(
            "Meanwhile {listed}. Pick {} in the header to read it.",
            if with_tokens.len() == 1 { "it" } else { "one" }
        )
    })
}

/// `1 attempt` / `19 attempts` — a count that reads as English at both ends.
fn attempts_phrase(attempts: usize) -> String {
    match attempts {
        1 => "1 attempt".to_string(),
        n => format!("{n} attempts"),
    }
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

    // -- which board the page is reading (gh#254) ----------------------------

    /// A board that answered a sweep: attempts, how many of them metered, and
    /// what they spent.
    fn board(
        device: Option<&str>,
        label: &str,
        attempts: usize,
        metered: usize,
        tokens: u64,
    ) -> HostBoard {
        HostBoard {
            device_id: device.map(str::to_string),
            label: label.into(),
            attempts,
            attempts_with_tokens: metered,
            tokens,
        }
    }

    /// The state this ticket was raised from, measured on the operator's own
    /// machine: the sweep resolves to the local board because it answers first,
    /// and the local board is the one with no tokens on it.
    #[test]
    fn an_empty_board_names_the_one_that_has_the_numbers() {
        let boards = vec![
            board(None, "this Mac", 19, 0, 0),
            board(Some("box"), "the box", 12, 6, 46_100_000),
        ];
        let note =
            other_boards_note(&boards, None).expect("two boards, so there is something to say");
        // Both halves: what is wrong with THIS board, and where the work is.
        assert!(
            note.contains("19 attempts, none of which recorded token usage"),
            "{note}"
        );
        assert!(
            note.contains("the board on the box has 46.10M tokens over 12 attempts"),
            "{note}"
        );
        assert!(note.contains("Pick it in the header"), "{note}");
    }

    /// Read from the other side, the same sweep has nothing to complain about
    /// — and the note still names the empty one rather than pretending the
    /// sweep found one board.
    #[test]
    fn the_note_is_written_from_whichever_board_is_on_screen() {
        let boards = vec![
            board(None, "this Mac", 19, 0, 0),
            board(Some("box"), "the box", 12, 6, 46_100_000),
        ];
        let note = other_boards_note(&boards, Some("box")).expect("still two boards");
        assert!(
            note.starts_with("The board on the box recorded 46.10M over 6 of 12 attempts."),
            "{note}"
        );
        assert!(
            note.contains("(this Mac) recorded no token usage either"),
            "{note}"
        );
    }

    /// One board is the ordinary install, and it is owed no comparison: the
    /// page keeps its own empty state instead of a sentence about nobody.
    #[test]
    fn a_lone_board_says_nothing_about_boards_it_did_not_find() {
        let boards = vec![board(None, "this Mac", 0, 0, 0)];
        assert_eq!(other_boards_note(&boards, None), None);
        // And a host the sweep never got an answer from has no note either —
        // there is no "here" to write the first half from.
        assert_eq!(other_boards_note(&boards, Some("ghost")), None);
    }

    /// Never dispatched from is a different fact from ran and never metered,
    /// and the note has to keep them apart — they call for different actions.
    #[test]
    fn a_board_nothing_ever_ran_on_says_that_rather_than_blaming_the_meter() {
        let quiet = board(None, "laptop", 0, 0, 0);
        assert_eq!(
            quiet.emptiness(),
            "Nothing has been dispatched from the board on laptop."
        );
        assert_eq!(
            board(None, "laptop", 1, 0, 0).emptiness(),
            "The board on laptop has 1 attempt, none of which recorded token usage."
        );
    }

    /// Biggest first among the boards that have data, so the one the work
    /// actually runs on is the one named first.
    #[test]
    fn the_fullest_other_board_is_named_first() {
        let boards = vec![
            board(None, "this Mac", 19, 0, 0),
            board(Some("small"), "old box", 4, 2, 3_200_000),
            board(Some("big"), "the box", 12, 6, 46_100_000),
        ];
        let note = other_boards_note(&boards, None).expect("three boards");
        let big = note.find("the box").expect("named");
        let small = note.find("old box").expect("named");
        assert!(big < small, "{note}");
        assert!(note.contains("Pick one in the header"), "{note}");
    }

    /// A board read off the wire keeps the three numbers the control needs and
    /// nothing else.
    #[test]
    fn a_host_board_is_read_off_the_window_it_answered_with() {
        let mut stats = BoardStats::empty(Some(7));
        stats.attempts = 19;
        let read = HostBoard::of(None, "this Mac", &stats);
        assert_eq!(read, board(None, "this Mac", 19, 0, 0));
        assert!(!read.has_tokens());
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

    /// gh#359: the row a *usage* view must not lose. Tokens are a fact the
    /// board knows exactly; only the money is unknown, and only the money says
    /// so.
    #[test]
    fn a_model_nobody_priced_keeps_its_row_and_says_which_half_is_missing() {
        let cut = rank_breakdown(
            Dimension::Model,
            vec![
                row("claude-opus-5", 9, 31_200_000, Some(198.0)),
                BreakdownRow {
                    unpriced_tokens: 2_900_000,
                    ..row("gpt-5.6-luna", 4, 2_900_000, Some(0.0))
                },
                // Alphabetically first of the two unpriced rows, and much the
                // smaller: it sorts under the one doing the work.
                BreakdownRow {
                    unpriced_tokens: 40_000,
                    ..row("codestral-3", 1, 40_000, Some(0.0))
                },
                row("claude-sonnet-5", 2, 3_400_000, Some(14.0)),
            ],
            BREAKDOWN_ROWS,
        );

        // Priced rows first, by money; the rest by tokens rather than by the
        // alphabet, which is the choice this ticket turns on.
        let labels: Vec<&str> = cut.rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "claude-opus-5",
                "claude-sonnet-5",
                "gpt-5.6-luna",
                "codestral-3"
            ]
        );

        // The row is whole: its tokens are its own, and where the money would
        // be it says which fact is missing rather than claiming a zero.
        let luna = &cut.rows[2];
        assert_eq!(luna.usage.total(), 2_900_000);
        assert!(luna.is_unpriced());
        assert_eq!(luna.price_label(), UNPRICED);
        assert_eq!(cut.rows[0].price_label(), "$198");
        // And the bar is left empty rather than drawn against a second
        // quantity: this cut is scaled in dollars.
        assert_eq!(cut.share(luna), 0.0);
    }

    /// The three states the money column has to keep apart, and the two it must
    /// never collapse into each other.
    #[test]
    fn the_money_column_says_nothing_metered_and_nothing_priceable_differently() {
        // Dispatches, no usage: no money and no tokens to price.
        let quiet = row("linear", 7, 0, Some(0.0));
        assert!(!quiet.is_unpriced());
        assert_eq!(quiet.price_label(), NO_FIGURE);

        // Metered, and free at the rates configured — a real figure.
        let free = row("mock", 2, 5_000, Some(0.0));
        assert!(!free.is_unpriced());
        assert_eq!(free.price_label(), "$0.00");

        // Priced work beside unpriced work: the figure is real and the card's
        // footer is what says what it leaves out.
        let mixed = BreakdownRow {
            unpriced_tokens: 900,
            ..row("codex", 4, 41_000, Some(1.20))
        };
        assert!(!mixed.is_unpriced());
        assert_eq!(mixed.price_label(), "$1.20");

        // No rates configured at all: the card drops the column, so the row
        // never has to word it.
        let unrated = row("claude-opus-5", 3, 9_000, None);
        assert!(!unrated.is_unpriced());
        assert_eq!(unrated.price_label(), NO_FIGURE);
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

    fn stats_device(id: &str, label: &str) -> StatsDevice {
        StatsDevice {
            device_id: id.to_string(),
            label: label.to_string(),
        }
    }

    fn snapshot(
        board_id: &str,
        host: StatsDevice,
        stats: BoardStats,
        durations: &[i64],
    ) -> BoardStatsSnapshot {
        let account_identities = stats
            .by_account
            .keys()
            .map(|label| {
                (
                    label.clone(),
                    StatsAccountIdentity::Shared {
                        account_id: label.clone(),
                    },
                )
            })
            .collect();
        BoardStatsSnapshot {
            board_id: board_id.to_string(),
            host,
            stats,
            merge_basis: StatsMergeBasis {
                duration_minutes: durations.to_vec(),
                breakdown: Vec::new(),
                account_identities,
            },
        }
    }

    #[test]
    fn aggregate_accounts_keep_local_logins_apart_and_merge_shared_payers() {
        fn account_stats(default_tokens: u64, shared_tokens: u64) -> BoardStats {
            let mut stats = BoardStats::empty(Some(7));
            stats.attempts = 2;
            stats.by_account = BTreeMap::from([
                ("the box's own login".into(), 1),
                ("shared@example.com".into(), 1),
            ]);
            stats.tokens_by_account = BTreeMap::from([
                ("the box's own login".into(), usage(default_tokens, 0, 0, 0)),
                ("shared@example.com".into(), usage(shared_tokens, 0, 0, 0)),
            ]);
            stats.spend = Some(BoardSpend {
                rates: RateTable::empty("2026-08-16"),
                list_price: Usd::ZERO,
                by_model: Vec::new(),
                unpriced: Vec::new(),
                unpriced_tokens: default_tokens + shared_tokens,
                accounts: stats
                    .by_account
                    .iter()
                    .map(|(label, attempts)| AccountSpend {
                        label: label.clone(),
                        attempts: *attempts,
                        usage: stats.tokens_by_account[label],
                        list_price: Usd::ZERO,
                        unpriced_tokens: stats.tokens_by_account[label].total(),
                        plan: None,
                        plan_in_window: None,
                    })
                    .collect(),
            });
            stats
        }

        fn identified_snapshot(board_id: &str, stats: BoardStats) -> BoardStatsSnapshot {
            let account_rows = stats
                .by_account
                .iter()
                .map(|(label, dispatches)| BreakdownRow {
                    label: label.clone(),
                    dispatches: *dispatches,
                    usage: stats.tokens_by_account[label],
                    cost: None,
                    unpriced_tokens: stats.tokens_by_account[label].total(),
                })
                .collect();
            BoardStatsSnapshot {
                board_id: board_id.into(),
                host: stats_device(board_id, board_id),
                stats,
                merge_basis: StatsMergeBasis {
                    breakdown: vec![Breakdown {
                        dimension: Dimension::Account,
                        rows: account_rows,
                        ranking: Ranking::Tokens,
                        peak: 0,
                    }],
                    account_identities: BTreeMap::from([
                        (
                            "the box's own login".into(),
                            StatsAccountIdentity::BoardLocal,
                        ),
                        (
                            "shared@example.com".into(),
                            StatsAccountIdentity::Shared {
                                account_id: "shared@example.com".into(),
                            },
                        ),
                    ]),
                    ..Default::default()
                },
            }
        }

        let aggregate = aggregate_board_stats(
            Some(7),
            vec![
                StatsProbe {
                    candidate: stats_device("a", "A"),
                    result: StatsProbeResult::Answered(identified_snapshot(
                        "board-a",
                        account_stats(10, 20),
                    )),
                },
                StatsProbe {
                    candidate: stats_device("b", "B"),
                    result: StatsProbeResult::Answered(identified_snapshot(
                        "board-b",
                        account_stats(30, 40),
                    )),
                },
            ],
        );
        assert_eq!(aggregate.stats.by_account["shared@example.com"], 2);
        assert_eq!(
            aggregate.stats.by_account["the box's own login (board-a)"],
            1
        );
        assert_eq!(
            aggregate.stats.by_account["the box's own login (board-b)"],
            1
        );
        assert_eq!(
            aggregate.stats.tokens_by_account["shared@example.com"].total(),
            60
        );
        assert_eq!(
            aggregate
                .stats
                .cut(Dimension::Account)
                .expect("account breakdown")
                .rows
                .iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            vec![
                "shared@example.com",
                "the box's own login (board-b)",
                "the box's own login (board-a)"
            ]
        );
        let labels = aggregate
            .stats
            .spend
            .expect("spend")
            .accounts
            .into_iter()
            .map(|row| row.label)
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            vec![
                "shared@example.com",
                "the box's own login (board-a)",
                "the box's own login (board-b)"
            ]
        );
    }

    #[test]
    fn all_boards_is_an_auditable_union_not_a_transport_sum() {
        let mac = stats_device("mac", "Mac");
        let alias = stats_device("mac-alias", "Mac via VPN");
        let cloud = stats_device("cloud", "Cloud box");
        let dark = stats_device("dark", "Offline laptop");

        let mut local = BoardStats::empty(Some(7));
        local.attempts = 2;
        local.tasks_touched = 1;
        local.outcomes.insert("done".into(), 2);
        local.total_minutes = 60;
        local.tokens = usage(100, 20, 0, 0);
        local.attempts_with_tokens = 2;
        local
            .tokens_by_model
            .insert("priced-model".into(), local.tokens);
        local.by_source.insert("github".into(), 2);
        local.by_account.insert("alice@example.com".into(), 2);
        local
            .tokens_by_account
            .insert("alice@example.com".into(), local.tokens);
        local.daily = vec![bucket("2026-08-15", 2, 2)];
        local.daily_tokens = vec![spent_on("2026-08-15", local.tokens.total())];
        local.spend = Some(split_of(vec![model(
            "priced-model",
            5.0,
            25.0,
            local.tokens,
        )]));
        let local_price = local.spend.as_ref().expect("priced").list_price;
        local.spend.as_mut().expect("priced").accounts = vec![AccountSpend {
            label: "alice@example.com".into(),
            attempts: 2,
            usage: local.tokens,
            list_price: local_price,
            unpriced_tokens: 0,
            plan: Some(AccountPlan {
                label: Some("Team seat".into()),
                monthly: Usd::from_dollars(20.0),
            }),
            plan_in_window: Some(Usd::from_dollars(4.666_667)),
        }];

        // The second store deliberately polls the same source. That is not an
        // alias: its attempts happened, even when the task names overlap.
        let mut remote = BoardStats::empty(Some(7));
        remote.attempts = 2;
        remote.tasks_touched = 1;
        remote.outcomes.insert("failed".into(), 1);
        remote.live = 1;
        remote.total_minutes = 100;
        remote.tokens = usage(800, 200, 0, 0);
        remote.attempts_with_tokens = 1;
        remote
            .tokens_by_model
            .insert("unknown-model".into(), remote.tokens);
        remote.by_source.insert("github".into(), 2);
        remote.by_account.insert("alice@example.com".into(), 2);
        remote
            .tokens_by_account
            .insert("alice@example.com".into(), remote.tokens);
        remote.spend = Some(BoardSpend {
            rates: RateTable::empty("2026-08-16"),
            list_price: Usd::ZERO,
            by_model: Vec::new(),
            unpriced: vec![TokenTally {
                label: "unknown-model".into(),
                usage: remote.tokens,
            }],
            unpriced_tokens: remote.tokens.total(),
            accounts: vec![AccountSpend {
                label: "alice@example.com".into(),
                attempts: 2,
                usage: remote.tokens,
                list_price: Usd::ZERO,
                unpriced_tokens: remote.tokens.total(),
                // The same subscription appears on both boards. The aggregate
                // keeps it once while adding the work attributed to it.
                plan: Some(AccountPlan {
                    label: Some("Team seat".into()),
                    monthly: Usd::from_dollars(20.0),
                }),
                plan_in_window: Some(Usd::from_dollars(4.666_667)),
            }],
        });
        remote.daily = vec![bucket("2026-08-15", 2, 0)];
        remote.daily_tokens = vec![spent_on("2026-08-15", remote.tokens.total())];

        let local_snapshot = snapshot("board-a", mac.clone(), local, &[10, 50]);
        let probes = vec![
            StatsProbe {
                candidate: mac,
                result: StatsProbeResult::Answered(local_snapshot.clone()),
            },
            StatsProbe {
                candidate: alias,
                // Same store through another path. Different contents would
                // still be ignored: board identity, not arrival order or row
                // similarity, is the deduplication key.
                result: StatsProbeResult::Answered(local_snapshot),
            },
            StatsProbe {
                candidate: cloud.clone(),
                result: StatsProbeResult::Answered(snapshot("board-b", cloud, remote, &[100])),
            },
            StatsProbe {
                candidate: dark,
                result: StatsProbeResult::Unreachable("timed out after 5s".into()),
            },
        ];

        let aggregate = aggregate_board_stats(Some(7), probes.clone());
        assert_eq!(aggregate.boards.len(), 2);
        assert_eq!(
            aggregate
                .boards
                .iter()
                .map(|board| board.board_id.as_str())
                .collect::<Vec<_>>(),
            vec!["board-a", "board-b"],
            "the existing local-first board selector keeps its tie-break"
        );
        assert_eq!(aggregate.stats.attempts, 4, "the alias is not counted");
        assert_eq!(
            aggregate.stats.tasks_touched, 2,
            "independent stores are not collapsed merely for polling one repo"
        );
        assert_eq!(aggregate.stats.outcomes.get("done"), Some(&2));
        assert_eq!(aggregate.stats.outcomes.get("failed"), Some(&1));
        assert_eq!(aggregate.stats.live, 1);
        assert_eq!(aggregate.stats.completion_rate, Some(2.0 / 3.0));
        assert_eq!(aggregate.stats.token_coverage, Some(0.75));
        assert_eq!(aggregate.stats.median_minutes, Some(50));
        assert_eq!(aggregate.stats.p90_minutes, Some(100));
        assert_eq!(aggregate.stats.longest_minutes, Some(100));
        assert_eq!(aggregate.stats.daily[0].dispatches, 4);
        assert_eq!(aggregate.stats.daily[0].done, 2);
        assert_eq!(
            aggregate.stats.by_account.get("alice@example.com"),
            Some(&4)
        );
        assert_eq!(
            aggregate.stats.tokens_by_account["alice@example.com"].total(),
            1_120
        );

        let spend = aggregate.stats.spend.as_ref().expect("one board had rates");
        assert!(spend.list_price > Usd::ZERO);
        assert_eq!(spend.unpriced_tokens, 1_000);
        assert_eq!(spend.unpriced[0].label, "unknown-model");
        assert_eq!(spend.accounts[0].attempts, 4);
        assert_eq!(spend.accounts[0].unpriced_tokens, 1_000);
        assert_eq!(
            spend.accounts[0]
                .plan
                .as_ref()
                .and_then(|plan| plan.label.as_deref()),
            Some("Team seat")
        );
        assert_eq!(spend.monthly_subscriptions(), Usd::from_dollars(20.0));
        assert_eq!(
            spend.subscriptions_in_window(),
            Some(Usd::from_dollars(4.666_667))
        );

        assert_eq!(aggregate.hosts[0].status, StatsHostStatus::Answered);
        assert_eq!(aggregate.hosts[1].status, StatsHostStatus::Duplicate);
        assert_eq!(aggregate.hosts[1].board_id.as_deref(), Some("board-a"));
        assert_eq!(aggregate.hosts[3].status, StatsHostStatus::Unreachable);
        assert!(!aggregate.complete);
        assert_eq!(
            aggregate.completeness_note().as_deref(),
            Some(
                "Partial aggregate — Offline laptop did not answer. The totals include only the boards that answered."
            )
        );

        // Concurrent replies must not leak arrival-order instability into the
        // JSON that the CLI and both viewports share.
        let again = aggregate_board_stats(Some(7), probes);
        assert_eq!(aggregate, again);
        let json = serde_json::to_string(&aggregate).expect("serializes");
        let decoded: AggregateBoardStats = serde_json::from_str(&json).expect("decodes");
        assert_eq!(decoded, aggregate);
        assert_eq!(serde_json::to_string(&again).expect("serializes"), json);
    }

    #[test]
    fn mixed_version_host_is_auditable_without_inventing_update_capability_or_legacy_totals() {
        let aggregate = aggregate_board_stats(
            Some(7),
            vec![StatsProbe {
                candidate: StatsDevice {
                    device_id: "tokenmaxxer".into(),
                    label: "Tokenmaxxer9000".into(),
                },
                result: StatsProbeResult::UpgradeRequired {
                    current_version: "0.7.1".into(),
                    required_version: "0.8.0".into(),
                    error: "unknown method: BoardStatsSnapshot".into(),
                    can_apply: false,
                },
            }],
        );

        assert!(!aggregate.complete);
        assert!(
            aggregate.boards.is_empty(),
            "legacy totals cannot be safely identified"
        );
        assert_eq!(aggregate.hosts[0].status, StatsHostStatus::UpgradeRequired);
        let upgrade = aggregate.hosts[0].upgrade.as_ref().unwrap();
        assert_eq!(upgrade.current_version, "0.7.1");
        assert_eq!(upgrade.required_version, "0.8.0");
        assert_eq!(upgrade.error, "unknown method: BoardStatsSnapshot");
        assert!(!upgrade.can_apply);
        assert!(
            aggregate
                .completeness_note()
                .unwrap()
                .contains("Tokenmaxxer9000 is on v0.7.1; v0.8.0 is required")
        );
        let json = serde_json::to_value(&aggregate).unwrap();
        assert_eq!(json["hosts"][0]["status"], "upgradeRequired");
        assert_eq!(
            json["hosts"][0]["upgrade"]["error"],
            "unknown method: BoardStatsSnapshot"
        );
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

    // -- what a mark says when asked (gh#469) ---------------------------------

    #[test]
    fn a_mark_detail_is_the_page_format_with_the_exact_count_beside_it() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(812), "812");
        assert_eq!(group_digits(1_310_442), "1,310,442");
        assert_eq!(tokens_line(812), "812 tokens", "no parenthesis when equal");
        assert_eq!(tokens_line(1_310_442), "1.31M tokens (1,310,442)");
        assert_eq!(mark_day("2026-08-05"), "Wed 5 Aug 2026");
        assert_eq!(
            mark_day("whenever"),
            "whenever",
            "unparseable stays as it came"
        );
        assert_eq!(hour_span(14), "14:00–15:00");
        assert_eq!(hour_span(23), "23:00–00:00", "the last hour wraps");
    }

    #[test]
    fn a_day_column_answers_with_both_of_its_series() {
        let daily = vec![bucket("2026-08-03", 4, 2), bucket("2026-08-04", 0, 0)];
        let columns = day_columns(&daily, &[spent_on("2026-08-03", 1_310_442)]);
        let busy = columns[0].detail();
        assert_eq!(busy.title, "Mon 3 Aug 2026");
        assert_eq!(
            busy.lines,
            vec!["1.31M tokens (1,310,442)", "4 dispatches"],
            "tokens and dispatches are both in the mark, so both are in the answer"
        );
        assert_eq!(
            busy.sentence(),
            "Mon 3 Aug 2026: 1.31M tokens (1,310,442), 4 dispatches"
        );
        // A quiet day is inspectable too, and says what its absence means
        // rather than printing a zero that would read as work that was free.
        let quiet = columns[1].detail();
        assert_eq!(quiet.lines, vec!["no tokens reported", "no dispatches"]);
    }

    #[test]
    fn a_dispatch_bar_names_its_stacked_series_apart() {
        let busy = day_bucket_detail(&bucket("2026-08-05", 8, 5));
        assert_eq!(busy.title, "Wed 5 Aug 2026");
        assert_eq!(busy.lines, vec!["8 dispatches", "5 ended done"]);
        let quiet = day_bucket_detail(&bucket("2026-08-04", 0, 0));
        assert_eq!(
            quiet.lines,
            vec!["no dispatches"],
            "an empty day does not claim a done share of nothing"
        );
    }

    #[test]
    fn a_heat_cell_answers_with_its_crossing_and_a_zero_is_still_an_answer() {
        let cell = hour_cell_detail("comet-native", 21, 3);
        assert_eq!(cell.title, "comet-native · 21:00–22:00");
        assert_eq!(cell.lines, vec!["3 dispatches"]);
        assert_eq!(
            hour_cell_detail("edge", 9, 0).lines,
            vec!["no dispatches"],
            "a cold cell is inspectable, not mute"
        );
    }

    #[test]
    fn a_cost_slice_answers_with_money_tokens_and_share() {
        let slice = CostSlice {
            class: CostClass::Output,
            cost: Usd::from_dollars(0.15),
            tokens: 6_100,
            share: 0.374,
        };
        let detail = slice.detail();
        assert_eq!(detail.title, "output");
        assert_eq!(
            detail.lines,
            vec![
                "$0.15 at list price",
                "6.1k tokens (6,100)",
                "37% of the priced total"
            ]
        );
    }

    #[test]
    fn a_landing_band_answers_with_its_share_and_an_empty_one_claims_none() {
        let landing = Landing {
            merged: 5,
            open: 2,
            closed_unmerged: 1,
            no_pr: 0,
            in_flight: 2,
        };
        let segments = landing.segments();
        let merged = segments[0].detail();
        assert_eq!(merged.title, "Merged");
        assert_eq!(merged.lines, vec!["5 tasks", "62% of what landed"]);
        let empty = segments[3].detail();
        assert_eq!(
            empty.lines,
            vec!["no tasks"],
            "a category with nothing in it says so and takes no share"
        );
    }

    #[test]
    fn a_breakdown_track_answers_in_the_money_columns_three_states() {
        let priced = BreakdownRow {
            label: "claude-opus-5".into(),
            dispatches: 8,
            usage: usage(9_000, 6_000, 148_000, 21_000),
            cost: Some(Usd::from_dollars(0.40)),
            unpriced_tokens: 0,
        };
        let detail = priced.detail(Dimension::Model);
        assert_eq!(detail.title, "claude-opus-5 · Model");
        assert_eq!(
            detail.lines,
            vec![
                "8 dispatches",
                "184k tokens (184,000)",
                "$0.40 at list price"
            ]
        );
        let unpriced = BreakdownRow {
            label: "gpt-5.6-luna".into(),
            dispatches: 4,
            usage: usage(400, 100, 0, 500),
            cost: Some(Usd::ZERO),
            unpriced_tokens: 1_000,
        };
        assert_eq!(
            unpriced.detail(Dimension::Model).lines,
            vec![
                "4 dispatches",
                "1.0k tokens (1,000)",
                "unpriced — no rate for this model"
            ],
            "no rate is a reason, never a blank or a $0.00"
        );
        let unmetered = BreakdownRow {
            label: "cursor".into(),
            dispatches: 2,
            usage: TokenUsage::default(),
            cost: None,
            unpriced_tokens: 0,
        };
        assert_eq!(
            unmetered.detail(Dimension::Runtime).lines,
            vec!["2 dispatches", "no token usage reported"]
        );
    }

    #[test]
    fn a_folded_breakdown_row_is_as_inspectable_as_the_rows_it_stands_for() {
        // The `n others` bucket a truncated cut ends in is a real bucket
        // (gh#469): the reader who hovers it gets its real totals, not a shrug.
        let rows: Vec<BreakdownRow> = ["a", "b", "c", "d"]
            .iter()
            .enumerate()
            .map(|(index, label)| BreakdownRow {
                label: (*label).to_string(),
                dispatches: 4 - index,
                usage: usage(100 * (4 - index as u64), 0, 0, 0),
                cost: None,
                unpriced_tokens: 0,
            })
            .collect();
        let cut = rank_breakdown(Dimension::Space, rows, 2);
        let folded = cut.rows.last().expect("the folded tail");
        let detail = folded.detail(Dimension::Space);
        assert_eq!(detail.title, "2 others · Space");
        assert_eq!(detail.lines, vec!["3 dispatches", "300 tokens"]);
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
        assert_eq!(json["pricingBasis"], "listPriceApiEstimate");
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
        older
            .as_object_mut()
            .expect("an object")
            .remove("pricingBasis");
        let back: BoardStats =
            serde_json::from_value(older.clone()).expect("deserializes without it");
        assert!(back.hours_by_workspace.is_empty());
        assert_eq!(back.pricing_basis, PricingBasis::ListPriceApiEstimate);
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
                // gh#426. Legacy money field names stay wire-compatible, but
                // every reply declares that they are list-price API estimates.
                "pricingBasis": stats.pricing_basis,
                // gh#426. The agent/model rows use an explicitly estimated
                // wire field, and the phone owns the same compact labels as
                // desktop rather than receiving pre-rendered prose.
                "agentLabels": stats.agent_usage.iter().map(AgentSpend::label).collect::<Vec<_>>(),
                "agentPriceLabels": stats.agent_usage.iter().map(AgentSpend::price_label).collect::<Vec<_>>(),
                // gh#228. The bar both viewports draw: four bands, the two
                // losses among them, and shares taken over what landed rather
                // than over what was touched.
                "landingHeadline": stats.landing.headline(),
                "landingSegments": stats.landing.segments(),
                "landingInFlightNote": stats.landing.in_flight_note(),
                // gh#469. What a tapped or hovered mark says: the phone's day
                // bars and landing bands read these, and the desktop derives
                // its own marks from the same module, so a bar can never say
                // one number on the Mac and another in a VoiceOver label.
                "dayBucketDetails": stats.daily.iter().map(day_bucket_detail).collect::<Vec<_>>(),
                "landingSegmentDetails": stats
                    .landing
                    .segments()
                    .iter()
                    .map(LandingSegment::detail)
                    .collect::<Vec<_>>(),
                // gh#271. The other meter: a window that reported nothing has
                // no share to show, and `0 of 0` would read as a board with no
                // context pressure rather than one with no measurements.
                "contextReported": stats.context.is_reported(),
            }
        })
    }

    /// The all-board envelope and the derivations every viewport reads from
    /// it. The merge itself is exercised exhaustively in `mod tests`; this
    /// case prevents the Rust JSON shape and the phone's decoder from drifting.
    fn aggregate_case(name: &str, aggregate: &AggregateBoardStats) -> Value {
        json!({
            "name": name,
            "aggregate": aggregate,
            "expect": {
                "boardCount": aggregate.boards.len(),
                "hostStatuses": aggregate.hosts.iter().map(|host| host.status).collect::<Vec<_>>(),
                "complete": aggregate.complete,
                "completenessNote": aggregate.completeness_note(),
                "attempts": aggregate.stats.attempts,
                "tokenTotal": aggregate.stats.tokens.total(),
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
            ("gpt-5.6-luna", usage(400, 100, 0, 500)),
        ]);

        // Context pressure (gh#271): fewer attempts reported a window than
        // reported tokens, which is the ordinary case — one of the three
        // harnesses meters no window at all — and the share is taken over the
        // ones that did, never over the week.
        busy.context = ContextPressure {
            attempts_reported: 6,
            near_compaction: 2,
            peak_percent: Some(97),
        };

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
                        label: "gpt-5.6-luna".into(),
                        dispatches: 5,
                        usage: usage(400, 100, 0, 500),
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
                ("gpt-5.6-luna", usage(400, 100, 0, 500)),
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
        priced.attempts_with_agent_usage = 1;
        priced.agent_usage = vec![
            AgentSpend {
                agent: AgentKind::Main,
                name: None,
                model: "claude-opus-5".into(),
                usage: usage(1_000, 200, 10_000, 300),
                list_price_api_estimate: Some(Usd::from_dollars(0.016_875)),
                unpriced_tokens: 0,
            },
            AgentSpend {
                agent: AgentKind::Subagent,
                name: Some("Explore".into()),
                model: "gpt-5.6-luna".into(),
                usage: usage(400, 100, 0, 500),
                list_price_api_estimate: Some(Usd::ZERO),
                unpriced_tokens: 1_000,
            },
        ];

        // Rates configured, and not one of them matched: a real answer, and not
        // the same one as "no rates configured" above.
        let mut nothing_priceable = BoardStats::empty(Some(7));
        nothing_priceable.attempts = 2;
        nothing_priceable.attempts_with_tokens = 2;
        nothing_priceable.token_coverage = Some(1.0);
        nothing_priceable.tokens = usage(400, 100, 0, 500);
        nothing_priceable.tokens_by_model =
            token_tally(&[("gpt-5.6-luna", usage(400, 100, 0, 500))]);
        nothing_priceable.spend = Some(spend(
            crate::view::rates::builtin(),
            &[("gpt-5.6-luna", usage(400, 100, 0, 500))],
            &[],
        ));

        // A window that priced most of itself and could not price one model
        // (gh#359) — the case Brede opened this on. The model keeps its row in
        // the cut, under the priced ones and ranked on the tokens it did spend,
        // and where its money would be the row says so. Both halves are here
        // because they have to agree: the footer's "not in that total" sentence
        // and the row are the same 2.90M tokens said twice.
        let mut unpriced_model = BoardStats::empty(Some(7));
        unpriced_model.attempts = 12;
        unpriced_model.tasks_touched = 10;
        unpriced_model.attempts_with_tokens = 12;
        unpriced_model.token_coverage = Some(1.0);
        unpriced_model.tokens = usage(309_000, 96_000, 2_548_000, 131_000);
        unpriced_model.tokens_by_model = token_tally(&[
            ("claude-opus-5", usage(9_000, 6_000, 148_000, 21_000)),
            ("gpt-5.6-luna", usage(300_000, 90_000, 2_400_000, 110_000)),
        ]);
        unpriced_model.breakdown = vec![rank_breakdown(
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
                    label: "gpt-5.6-luna".into(),
                    dispatches: 4,
                    usage: usage(300_000, 90_000, 2_400_000, 110_000),
                    cost: Some(Usd::ZERO),
                    unpriced_tokens: 2_900_000,
                },
            ],
            BREAKDOWN_ROWS,
        )];
        unpriced_model.spend = Some(spend(
            crate::view::rates::builtin(),
            &[
                ("claude-opus-5", usage(9_000, 6_000, 148_000, 21_000)),
                ("gpt-5.6-luna", usage(300_000, 90_000, 2_400_000, 110_000)),
            ],
            &[],
        ));

        // Two real stores, one alias path to the first, and one host that did
        // not answer. This is the entire gh#461 contract in one decode case:
        // board identity deduplicates transport, independent stores add, and
        // partial collection is never mistaken for a complete zero.
        let mut fixture_board_a = BoardStats::empty(Some(7));
        fixture_board_a.attempts = 2;
        fixture_board_a.tasks_touched = 1;
        fixture_board_a.outcomes.insert("done".into(), 2);
        fixture_board_a.tokens = usage(100, 20, 0, 0);
        fixture_board_a.attempts_with_tokens = 2;
        fixture_board_a.token_coverage = Some(1.0);
        let mut fixture_board_b = BoardStats::empty(Some(7));
        fixture_board_b.attempts = 1;
        fixture_board_b.tasks_touched = 1;
        fixture_board_b.live = 1;
        let aggregate = aggregate_board_stats(
            Some(7),
            vec![
                StatsProbe {
                    candidate: StatsDevice {
                        device_id: "mac".into(),
                        label: "Mac".into(),
                    },
                    result: StatsProbeResult::Answered(BoardStatsSnapshot {
                        board_id: "board-a".into(),
                        host: StatsDevice {
                            device_id: "mac".into(),
                            label: "Mac".into(),
                        },
                        stats: fixture_board_a.clone(),
                        merge_basis: StatsMergeBasis {
                            duration_minutes: vec![18, 74],
                            breakdown: Vec::new(),
                            ..Default::default()
                        },
                    }),
                },
                StatsProbe {
                    candidate: StatsDevice {
                        device_id: "mac-vpn".into(),
                        label: "Mac via VPN".into(),
                    },
                    result: StatsProbeResult::Answered(BoardStatsSnapshot {
                        board_id: "board-a".into(),
                        host: StatsDevice {
                            device_id: "mac".into(),
                            label: "Mac".into(),
                        },
                        stats: fixture_board_a,
                        merge_basis: StatsMergeBasis::default(),
                    }),
                },
                StatsProbe {
                    candidate: StatsDevice {
                        device_id: "cloud".into(),
                        label: "Cloud box".into(),
                    },
                    result: StatsProbeResult::Answered(BoardStatsSnapshot {
                        board_id: "board-b".into(),
                        host: StatsDevice {
                            device_id: "cloud".into(),
                            label: "Cloud box".into(),
                        },
                        stats: fixture_board_b,
                        merge_basis: StatsMergeBasis {
                            duration_minutes: vec![5],
                            breakdown: Vec::new(),
                            ..Default::default()
                        },
                    }),
                },
                StatsProbe {
                    candidate: StatsDevice {
                        device_id: "dark".into(),
                        label: "Offline laptop".into(),
                    },
                    result: StatsProbeResult::Unreachable("timed out after 5s".into()),
                },
            ],
        );
        // The mixed-version rollout fixture has two frames: the v0.7-shaped
        // peer lacks BoardStatsSnapshot but still answers UpdateStatus, then
        // the same peer returns with a stable board id after an out-of-band
        // update and relay restart. v0.7 has no acceptance-equivalent install
        // capability, so this fixture must not invent one; gh#486 adds it for
        // future N-1 releases. Legacy BoardStats is intentionally not merged.
        let mixed_version = aggregate_board_stats(
            Some(7),
            vec![StatsProbe {
                candidate: StatsDevice {
                    device_id: "tokenmaxxer".into(),
                    label: "Tokenmaxxer9000".into(),
                },
                result: StatsProbeResult::UpgradeRequired {
                    current_version: "0.7.1".into(),
                    required_version: "0.8.0".into(),
                    error: "unknown method: BoardStatsSnapshot".into(),
                    can_apply: false,
                },
            }],
        );
        let mut updated_stats = BoardStats::empty(Some(7));
        updated_stats.attempts = 89;
        let after_update = aggregate_board_stats(
            Some(7),
            vec![StatsProbe {
                candidate: StatsDevice {
                    device_id: "tokenmaxxer".into(),
                    label: "Tokenmaxxer9000".into(),
                },
                result: StatsProbeResult::Answered(BoardStatsSnapshot {
                    board_id: "tokenmaxxer-board".into(),
                    host: StatsDevice {
                        device_id: "tokenmaxxer".into(),
                        label: "Tokenmaxxer9000".into(),
                    },
                    stats: updated_stats,
                    merge_basis: StatsMergeBasis::default(),
                }),
            }],
        );

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
                stats_case("one model the table has no rate for", &unpriced_model),
            ],
            "aggregateStats": [
                aggregate_case("two boards, an alias, and an unreachable host", &aggregate),
                aggregate_case("v0.7 peer requires an update without unsafe legacy totals", &mixed_version),
                aggregate_case("updated peer returns after relay restart", &after_update),
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
