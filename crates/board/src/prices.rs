//! What this board prices with (gh#182) — the rates, and the plans they are
//! read against.
//!
//! The *types* and the rate table live in [`comet_proto::view::rates`], beside
//! the stats shape both viewports read. What lives here is the half that is
//! about this box: which rates it is running (the shipped table plus whatever
//! `routing.toml` overrides), what its operator says the subscriptions cost,
//! and the arithmetic that turns a window's tokens into the two figures the
//! stats page keeps apart.
//!
//! **Two facts, two places, on purpose.** Rates are the board's own knowledge
//! and go in `[defaults.rates]`, which is already forwardable and validated
//! like every other board default. A subscription's cost is one person's and
//! goes in `[account."<slot>"]`, beside the slot it describes. Rolling them
//! into one table would be convenient exactly until a second teammate's slot
//! landed on the box (gh#59) and the board started adding up other people's
//! plans as if they were its own spend.

use std::collections::BTreeMap;

use comet_proto::TokenUsage;
use comet_proto::view::rates::{RateTable, Usd};
use comet_proto::view::stats::{AccountPlan, AccountSpend, BoardSpend, ModelSpend, TokenTally};

use crate::config::RoutingConfig;

/// A month, for pro-rating a monthly plan across a window of days.
///
/// Thirty flat, not the calendar's 28–31: the figure it produces is read
/// against a rolling "last 7 days" window that does not line up with a month
/// either, and a leap-year-accurate divisor would imply a precision the whole
/// comparison does not have.
const DAYS_PER_MONTH: f64 = 30.0;

/// What this board prices with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prices {
    /// The shipped table plus config overrides — see
    /// [`comet_proto::view::rates`].
    pub table: RateTable,
    /// The subscriptions an operator has written down, in `routing.toml` order.
    pub plans: Vec<SlotPlan>,
}

/// One agent-account slot's plan, as configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotPlan {
    /// The `[account."…"]` key: an agent-account slot id, or the email on it.
    pub slot: String,
    /// The login's email, when the key was a slot id. What actually matches an
    /// attempt row — the board records *who was billed*, and a slot id never
    /// appears there.
    pub email: Option<String>,
    /// What the plan is called, in the operator's words.
    pub label: Option<String>,
    pub monthly: Usd,
}

impl SlotPlan {
    /// Does this plan describe the account a stats row is keyed on? Either
    /// spelling counts, case-insensitively, because the board sees the email
    /// and the operator may well have written the slot id.
    fn describes(&self, account: &str) -> bool {
        let account = account.trim();
        self.slot.eq_ignore_ascii_case(account)
            || self
                .email
                .as_deref()
                .is_some_and(|e| e.eq_ignore_ascii_case(account))
    }
}

impl Prices {
    /// What a board with no config at all prices with: the shipped table, and
    /// nobody's plan.
    pub fn builtin() -> Self {
        Prices {
            table: comet_proto::view::rates::builtin(),
            plans: Vec::new(),
        }
    }

    /// The rates and plans this `routing.toml` describes.
    pub fn from_config(cfg: &RoutingConfig) -> Self {
        Prices {
            table: comet_proto::view::rates::builtin().with_overrides(
                cfg.defaults
                    .rates
                    .iter()
                    .map(|(m, r)| (m.clone(), r.rate())),
            ),
            plans: cfg
                .accounts
                .iter()
                .map(|(slot, account)| SlotPlan {
                    slot: slot.clone(),
                    email: account
                        .email
                        .as_deref()
                        .map(str::trim)
                        .filter(|e| !e.is_empty())
                        .map(str::to_string),
                    label: account
                        .plan
                        .as_deref()
                        .map(str::trim)
                        .filter(|p| !p.is_empty())
                        .map(str::to_string),
                    monthly: account.monthly_usd,
                })
                .collect(),
        }
    }

    fn plan_for(&self, account: &str) -> Option<&SlotPlan> {
        self.plans.iter().find(|p| p.describes(account))
    }

    /// Price a window.
    ///
    /// `None` is **rates not configured** — the state a page says out loud
    /// instead of rendering a confident zero. Everything else is a `Some`,
    /// including a window that spent nothing and a window whose every model was
    /// one the table has never heard of: those are honest zeroes with the
    /// unpriced tokens named beside them.
    ///
    /// `tokens_by_account_model` is what makes the per-account figures exact:
    /// an account's tokens are priced with the rates of the models it actually
    /// ran, never at some board-wide average. Totals are rounded to the
    /// microdollar as they are summed, so the account rows agree with the
    /// headline to far past the cent anybody reads.
    pub fn spend(
        &self,
        tokens_by_model: &BTreeMap<String, TokenUsage>,
        by_account: &BTreeMap<String, usize>,
        tokens_by_account_model: &BTreeMap<(String, String), TokenUsage>,
        since_days: Option<i64>,
    ) -> Option<BoardSpend> {
        if self.table.is_empty() {
            return None;
        }

        // Per model: priced where there is a rate, and visibly unpriced where
        // there is not. A model that spent nothing is not a row in a table
        // about spending, which is `ranked_tokens`' rule and the same one here.
        let mut by_model: Vec<ModelSpend> = Vec::new();
        let mut unpriced: Vec<TokenTally> = Vec::new();
        for (label, usage) in tokens_by_model.iter().filter(|(_, u)| !u.is_zero()) {
            match self.table.rate_for(label) {
                Some(found) => by_model.push(ModelSpend {
                    label: label.clone(),
                    rate_key: found.key,
                    source: found.source,
                    rate: found.rate,
                    usage: *usage,
                    cost: found.rate.cost(*usage),
                }),
                None => unpriced.push(TokenTally {
                    label: label.clone(),
                    usage: *usage,
                }),
            }
        }
        by_model.sort_by(|a, b| b.cost.cmp(&a.cost).then_with(|| a.label.cmp(&b.label)));
        unpriced.sort_by(|a, b| {
            b.usage
                .total()
                .cmp(&a.usage.total())
                .then_with(|| a.label.cmp(&b.label))
        });

        let list_price: Usd = by_model.iter().map(|m| m.cost).sum();
        let unpriced_tokens: u64 = unpriced.iter().map(|t| t.usage.total()).sum();

        // Per account: the same pricing, applied to the split that says whose
        // subscription paid for which model's tokens.
        let mut accounts: Vec<AccountSpend> = by_account
            .iter()
            .map(|(account, attempts)| {
                let mut usage = TokenUsage::default();
                let mut priced = Usd::ZERO;
                let mut unpriced_here = 0u64;
                for ((who, model), spent) in tokens_by_account_model {
                    if who != account {
                        continue;
                    }
                    usage.add(*spent);
                    match self.table.rate_for(model) {
                        Some(found) => priced += found.rate.cost(*spent),
                        None => unpriced_here += spent.total(),
                    }
                }
                let plan = self.plan_for(account).map(|p| AccountPlan {
                    label: p.label.clone(),
                    monthly: p.monthly,
                });
                AccountSpend {
                    label: account.clone(),
                    attempts: *attempts,
                    usage,
                    list_price: priced,
                    unpriced_tokens: unpriced_here,
                    plan_in_window: plan.as_ref().and_then(|p| prorate(p.monthly, since_days)),
                    plan,
                }
            })
            .collect();
        accounts.sort_by(|a, b| {
            b.list_price
                .cmp(&a.list_price)
                .then_with(|| a.label.cmp(&b.label))
        });

        Some(BoardSpend {
            rates: self.table.clone(),
            list_price,
            by_model,
            unpriced,
            unpriced_tokens,
            accounts,
        })
    }
}

/// A monthly plan's share of a window of `days`.
///
/// `None` for an all-time window, which is the honest answer: "how far did the
/// subscription carry it" needs a period to be asked about, and all time is
/// not one. A plan that costs nothing pro-rates to nothing rather than to
/// `None` — free is a real answer.
fn prorate(monthly: Usd, since_days: Option<i64>) -> Option<Usd> {
    let days = since_days?.max(0) as f64;
    Some(Usd::from_micros(
        (monthly.micros as f64 * days / DAYS_PER_MONTH).round() as i64,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use comet_proto::view::rates::{ModelRate, RateSource};

    fn usage(input: u64, output: u64, read: u64, write: u64) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: read,
            cache_creation_tokens: write,
        }
    }

    fn models(pairs: &[(&str, TokenUsage)]) -> BTreeMap<String, TokenUsage> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
    }

    fn accounts(pairs: &[(&str, usize)]) -> BTreeMap<String, usize> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
    }

    fn split(pairs: &[(&str, &str, TokenUsage)]) -> BTreeMap<(String, String), TokenUsage> {
        pairs
            .iter()
            .map(|(a, m, u)| (((*a).to_string(), (*m).to_string()), *u))
            .collect()
    }

    #[test]
    fn a_board_with_no_rates_says_so_rather_than_pricing_at_zero() {
        let none = Prices {
            table: RateTable::empty("2026-06-24"),
            plans: Vec::new(),
        };
        let spent = models(&[("claude-opus-5", usage(1_000, 100, 0, 0))]);
        assert_eq!(
            none.spend(&spent, &BTreeMap::new(), &BTreeMap::new(), Some(7)),
            None,
            "unconfigured is not a zero"
        );

        // And a board WITH rates that simply spent nothing is a different fact:
        // a real total, which happens to be zero.
        let quiet = Prices::builtin()
            .spend(
                &BTreeMap::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                Some(7),
            )
            .expect("rates are configured");
        assert_eq!(quiet.list_price, Usd::ZERO);
        assert!(!quiet.has_price());
        assert!(quiet.is_complete(), "nothing to leave out");
    }

    /// The rule the whole feature turns on: a model with no rate is carried
    /// through as unpriced, never dropped and never summed in as free.
    #[test]
    fn a_model_with_no_rate_is_visible_rather_than_missing() {
        let spent = models(&[
            ("claude-opus-5", usage(10_000, 2_000, 1_000_000, 20_000)),
            ("gpt-5.6-terra", usage(400, 100, 0, 500)),
            ("unnamed model", usage(7, 1, 0, 0)),
        ]);
        let spend = Prices::builtin()
            .spend(&spent, &BTreeMap::new(), &BTreeMap::new(), Some(7))
            .expect("rates are configured");

        assert_eq!(spend.by_model.len(), 1);
        assert_eq!(spend.by_model[0].label, "claude-opus-5");
        assert_eq!(spend.by_model[0].cost, Usd::from_dollars(0.725));
        assert_eq!(spend.list_price, Usd::from_dollars(0.725));

        // Biggest unpriced first, and the total says how much the price above
        // does not account for.
        let unpriced: Vec<&str> = spend.unpriced.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(unpriced, ["gpt-5.6-terra", "unnamed model"]);
        assert_eq!(spend.unpriced_tokens, 1_008);
        assert!(!spend.is_complete());
        assert!(spend.headline().contains("unpriced"));
    }

    #[test]
    fn priced_models_read_biggest_first_and_ties_do_not_shuffle() {
        let spent = models(&[
            ("claude-haiku-4-5", usage(1_000_000, 0, 0, 0)), // $1.00
            ("claude-opus-5", usage(1_000_000, 0, 0, 0)),    // $5.00
            ("claude-sonnet-5", usage(1_000_000, 0, 0, 0)),  // $3.00
        ]);
        let spend = Prices::builtin()
            .spend(&spent, &BTreeMap::new(), &BTreeMap::new(), None)
            .expect("rates are configured");
        let order: Vec<&str> = spend.by_model.iter().map(|m| m.label.as_str()).collect();
        assert_eq!(
            order,
            ["claude-opus-5", "claude-sonnet-5", "claude-haiku-4-5"]
        );
        assert_eq!(spend.list_price, Usd::from_dollars(9.0));
        // Every row carries the rate it was priced at, so the number is
        // checkable without leaving the page.
        assert_eq!(spend.by_model[0].rate.input, Usd::from_dollars(5.0));
        assert_eq!(spend.by_model[0].source, RateSource::Builtin);
        assert_eq!(spend.by_model[0].rate_key, "claude-opus-5");
    }

    #[test]
    fn a_dated_model_prices_off_its_family_and_says_which_row_it_used() {
        let spent = models(&[("claude-opus-5-20260101", usage(1_000_000, 0, 0, 0))]);
        let spend = Prices::builtin()
            .spend(&spent, &BTreeMap::new(), &BTreeMap::new(), None)
            .expect("rates are configured");
        let row = &spend.by_model[0];
        assert_eq!(row.label, "claude-opus-5-20260101", "the run's own word");
        assert_eq!(row.rate_key, "claude-opus-5", "the row that priced it");
        assert_eq!(row.cost, Usd::from_dollars(5.0));
    }

    /// The second fact, kept apart from the first: what the board ran is priced
    /// per account, and what the operator pays is a separate figure that is
    /// never added into it.
    #[test]
    fn a_subscription_is_reported_beside_the_list_price_and_never_added_to_it() {
        let prices = Prices {
            table: comet_proto::view::rates::builtin(),
            plans: vec![
                SlotPlan {
                    slot: "8f2c1d0a7b6e4539".into(),
                    email: Some("brede@tally.no".into()),
                    label: Some("Claude Max 20x".into()),
                    monthly: Usd::from_dollars(200.0),
                },
                SlotPlan {
                    slot: "ana@example.com".into(),
                    email: None,
                    label: None,
                    monthly: Usd::from_dollars(20.0),
                },
            ],
        };
        let spent = models(&[("claude-opus-5", usage(2_000_000, 0, 0, 0))]);
        let counts = accounts(&[("brede@tally.no", 9), ("ana@example.com", 2)]);
        let by_pair = split(&[
            ("brede@tally.no", "claude-opus-5", usage(1_800_000, 0, 0, 0)),
            ("ana@example.com", "claude-opus-5", usage(200_000, 0, 0, 0)),
        ]);
        let spend = prices
            .spend(&spent, &counts, &by_pair, Some(30))
            .expect("rates are configured");

        // Biggest spender first, matched by slot id and by email alike.
        assert_eq!(spend.accounts[0].label, "brede@tally.no");
        assert_eq!(spend.accounts[0].list_price, Usd::from_dollars(9.0));
        assert_eq!(
            spend.accounts[0].plan.as_ref().map(|p| p.monthly),
            Some(Usd::from_dollars(200.0))
        );
        assert_eq!(spend.accounts[0].attempts, 9);
        assert_eq!(spend.accounts[1].label, "ana@example.com");
        assert_eq!(spend.accounts[1].list_price, Usd::from_dollars(1.0));

        // The account rows account for the headline, and the plans are nowhere
        // in it: one is the board's spend, the other is two people's bills.
        assert_eq!(spend.list_price, Usd::from_dollars(10.0));
        assert_eq!(
            spend.accounts.iter().map(|a| a.list_price).sum::<Usd>(),
            spend.list_price
        );
        assert_eq!(spend.monthly_subscriptions(), Usd::from_dollars(220.0));
    }

    #[test]
    fn the_subsidy_is_a_ratio_over_a_window_and_all_time_has_none() {
        let prices = Prices {
            table: comet_proto::view::rates::builtin(),
            plans: vec![SlotPlan {
                slot: "brede@tally.no".into(),
                email: None,
                label: Some("Max".into()),
                monthly: Usd::from_dollars(300.0),
            }],
        };
        let spent = models(&[("claude-opus-5", usage(20_000_000, 0, 0, 0))]); // $100
        let counts = accounts(&[("brede@tally.no", 4)]);
        let by_pair = split(&[(
            "brede@tally.no",
            "claude-opus-5",
            usage(20_000_000, 0, 0, 0),
        )]);

        // A month's window: the whole plan.
        let month = prices
            .spend(&spent, &counts, &by_pair, Some(30))
            .expect("configured");
        assert_eq!(
            month.accounts[0].plan_in_window,
            Some(Usd::from_dollars(300.0))
        );
        assert_eq!(month.accounts[0].subsidy(), Some(1.0 / 3.0));

        // A week's: a week's share of it.
        let week = prices
            .spend(&spent, &counts, &by_pair, Some(7))
            .expect("configured");
        assert_eq!(
            week.accounts[0].plan_in_window,
            Some(Usd::from_dollars(70.0))
        );

        // All time cannot be pro-rated, and says so rather than inventing a
        // period to divide by.
        let ever = prices
            .spend(&spent, &counts, &by_pair, None)
            .expect("configured");
        assert_eq!(ever.accounts[0].plan_in_window, None);
        assert_eq!(ever.accounts[0].subsidy(), None);
        // The plan itself is still reported — only the ratio is unanswerable.
        assert!(ever.accounts[0].plan.is_some());
    }

    #[test]
    fn an_account_nobody_configured_a_plan_for_is_unpriced_not_free() {
        let prices = Prices::builtin();
        let spent = models(&[("claude-opus-5", usage(1_000_000, 0, 0, 0))]);
        let counts = accounts(&[("the box's own login", 3)]);
        let by_pair = split(&[(
            "the box's own login",
            "claude-opus-5",
            usage(1_000_000, 0, 0, 0),
        )]);
        let spend = prices
            .spend(&spent, &counts, &by_pair, Some(7))
            .expect("configured");
        let row = &spend.accounts[0];
        assert_eq!(
            row.list_price,
            Usd::from_dollars(5.0),
            "what it ran is known"
        );
        assert_eq!(row.plan, None, "what it costs is not");
        assert_eq!(row.plan_in_window, None);
        assert_eq!(row.subsidy(), None);
        assert_eq!(spend.monthly_subscriptions(), Usd::ZERO);
    }

    #[test]
    fn an_accounts_unpriced_models_are_named_on_its_own_row() {
        let prices = Prices::builtin();
        let spent = models(&[
            ("claude-opus-5", usage(1_000_000, 0, 0, 0)),
            ("gpt-5.6-terra", usage(500, 100, 0, 0)),
        ]);
        let counts = accounts(&[("ana@example.com", 2)]);
        let by_pair = split(&[
            (
                "ana@example.com",
                "claude-opus-5",
                usage(1_000_000, 0, 0, 0),
            ),
            ("ana@example.com", "gpt-5.6-terra", usage(500, 100, 0, 0)),
        ]);
        let spend = prices
            .spend(&spent, &counts, &by_pair, Some(7))
            .expect("configured");
        assert_eq!(spend.accounts[0].list_price, Usd::from_dollars(5.0));
        assert_eq!(spend.accounts[0].unpriced_tokens, 600);
        assert_eq!(spend.accounts[0].usage.total(), 1_000_600);
    }

    #[test]
    fn an_override_is_what_prices_a_model_the_table_never_heard_of() {
        let prices = Prices {
            table: comet_proto::view::rates::builtin().with_overrides([(
                "gpt-5.6-terra".to_string(),
                ModelRate::published(1.25, 10.0),
            )]),
            plans: Vec::new(),
        };
        let spent = models(&[("gpt-5.6-terra", usage(1_000_000, 100_000, 0, 0))]);
        let spend = prices
            .spend(&spent, &BTreeMap::new(), &BTreeMap::new(), Some(7))
            .expect("configured");
        assert_eq!(spend.by_model[0].source, RateSource::Config);
        assert_eq!(spend.by_model[0].cost, Usd::from_dollars(2.25));
        assert!(spend.is_complete(), "nothing left unpriced");
    }
}
