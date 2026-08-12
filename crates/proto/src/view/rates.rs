//! What a token costs at the meter (gh#182) — the rate table, and the money
//! type every surface adds up in.
//!
//! The board counts tokens (gh#151); this is what prices them. It lives beside
//! [`super::stats`] and for the same reason: the number ends up on a page the
//! phone renders as well as the laptop, and two implementations of a rate
//! lookup is two boards quietly disagreeing about money.
//!
//! Three decisions this module owns, in the order they matter:
//!
//! **Four rates per model, never one.** [`crate::TokenUsage`]'s four buckets
//! are priced nowhere near each other — a cache read is a tenth of fresh
//! input, a cache write a quarter more than it — and a coding agent replaying
//! a long context every turn is overwhelmingly cache reads. A single $/token
//! rate would not be a rounding error here; it would be wrong by a multiple.
//!
//! **A dated table shipped with the binary**, not a runtime lookup. Neither
//! provider publishes a pricing API: pricing lives on a docs page, and the
//! programmatic sources are third-party aggregators. Scraping HTML for money,
//! on a box with no network guarantees, fails silently into a number nobody
//! re-checks. So the table carries [`RateTable::as_of`] and says how old it is
//! ([`RateTable::is_stale`]); `doctor` reports that date rather than implying
//! freshness. Refreshing it is a diff a person merges — which is the shape of
//! everything else here — and the automation for that is a follow-up, not this.
//!
//! **A model with no rate is *unpriced*, never zero.** [`RateTable::rate_for`]
//! answers `None` and the caller carries the tokens through as unpriced
//! (gh#96's lesson applied to money): a total that silently swallowed a model
//! it could not price is worse than a total that says what it left out.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::TokenUsage;

// ---------------------------------------------------------------------------
// Money
// ---------------------------------------------------------------------------

/// An amount in US dollars, kept to the microdollar.
///
/// Integer micros rather than a float because this is money that gets summed
/// over hundreds of rows: floats accumulate a cent of drift and then a table
/// does not add up to its own total, which is the one thing a spend page must
/// never do. It is also what lets [`super::stats::BoardStats`] keep its derived
/// `Eq` — a config full of `f64` would have taken that away from the type the
/// board loop compares against disk (gh#189).
///
/// On the wire it is a plain number of dollars (`4.28`), because the number a
/// person checks `comet-board stats --json` against is dollars. The conversion
/// is exact in both directions for every amount a board will ever hold.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Usd {
    /// Millionths of a dollar.
    pub micros: i64,
}

/// One dollar, in micros — the scale everything below converts through.
const MICROS: i64 = 1_000_000;

impl Usd {
    pub const ZERO: Usd = Usd { micros: 0 };

    pub const fn from_micros(micros: i64) -> Self {
        Usd { micros }
    }

    /// From a written amount — a config value, a table entry.
    pub fn from_dollars(dollars: f64) -> Self {
        if !dollars.is_finite() {
            return Usd::ZERO;
        }
        Usd {
            micros: (dollars * MICROS as f64).round() as i64,
        }
    }

    pub fn dollars(self) -> f64 {
        self.micros as f64 / MICROS as f64
    }

    pub fn is_zero(self) -> bool {
        self.micros == 0
    }

    /// This rate applied to `tokens`, where the rate is per *million* tokens.
    ///
    /// Done in `i128` and rounded once at the end: a board's yearly total is
    /// well inside `i64` micros, but `tokens × micros` on the way there is not.
    pub fn per_million(self, tokens: u64) -> Usd {
        let scaled = (self.micros as i128) * (tokens as i128);
        let rounded = (scaled + 500_000) / 1_000_000;
        Usd {
            micros: rounded.clamp(i64::MIN as i128, i64::MAX as i128) as i64,
        }
    }
}

impl std::ops::Add for Usd {
    type Output = Usd;
    fn add(self, rhs: Usd) -> Usd {
        Usd {
            micros: self.micros.saturating_add(rhs.micros),
        }
    }
}

impl std::ops::AddAssign for Usd {
    fn add_assign(&mut self, rhs: Usd) {
        *self = *self + rhs;
    }
}

impl std::iter::Sum for Usd {
    fn sum<I: Iterator<Item = Usd>>(iter: I) -> Usd {
        iter.fold(Usd::ZERO, |a, b| a + b)
    }
}

impl Serialize for Usd {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_f64(self.dollars())
    }
}

impl<'de> Deserialize<'de> for Usd {
    /// Accepts `200`, `200.0` and `199.99` alike. A hand-written config is
    /// where most of these come from, and refusing `monthly_usd = 200` on the
    /// grounds that money is a float would be a papercut with no upside.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Usd, D::Error> {
        struct Amount;
        impl serde::de::Visitor<'_> for Amount {
            type Value = Usd;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an amount in US dollars")
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Usd, E> {
                Ok(Usd::from_dollars(v))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Usd, E> {
                Ok(Usd::from_dollars(v as f64))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Usd, E> {
                Ok(Usd::from_dollars(v as f64))
            }
        }
        d.deserialize_any(Amount)
    }
}

/// An amount as a page shows it: `$0.0042`, `$1.32`, `$248`.
///
/// Three tiers, because the same field carries a per-model row and a headline.
/// Cents is the default scale money is read at; over a hundred dollars they are
/// noise on a figure nobody acts on to the cent, and under one cent they are
/// the difference between a real amount and a `$0.00` that reads as free —
/// which is the one thing this whole module exists to avoid printing.
pub fn human_usd(amount: Usd) -> String {
    let d = amount.dollars();
    let abs = d.abs();
    if abs >= 100.0 {
        format!("${d:.0}")
    } else if abs >= 0.01 || abs == 0.0 {
        format!("${d:.2}")
    } else {
        format!("${d:.4}")
    }
}

// ---------------------------------------------------------------------------
// Rates
// ---------------------------------------------------------------------------

/// What one model costs, per million tokens, in each of the four buckets
/// [`TokenUsage`] counts.
///
/// The bucket names are the *usage*'s names on purpose: `cache_write` here
/// prices `cache_creation_tokens`, and a rate table whose fields did not line
/// up one-to-one with the counter would invite exactly the mix-up this module
/// exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRate {
    /// Fresh input.
    pub input: Usd,
    pub output: Usd,
    /// Input served out of the prompt cache — a fraction of `input`.
    pub cache_read: Usd,
    /// Input written *into* the cache — a premium on `input`.
    pub cache_write: Usd,
}

impl ModelRate {
    /// A rate written the way a price list writes it: dollars per million
    /// tokens for input and output, with the cache rates derived from input at
    /// the published multipliers (a read is 0.1×, a five-minute write 1.25×).
    ///
    /// Anthropic publishes the multipliers rather than a per-model cache price,
    /// so deriving them is what the table *is* — writing four literals per row
    /// would only be an opportunity to fat-finger one of them.
    pub fn published(input: f64, output: f64) -> Self {
        ModelRate {
            input: Usd::from_dollars(input),
            output: Usd::from_dollars(output),
            cache_read: Usd::from_dollars(input * 0.1),
            cache_write: Usd::from_dollars(input * 1.25),
        }
    }

    /// A rate where the cache tiers are published explicitly, because the
    /// provider's multipliers do not match the Anthropic defaults (0.1× read,
    /// 1.25× write). DeepSeek V4 Flash, for example, charges 0.02× input for a
    /// cache read rather than 0.1×, and does not publish a separate cache-write
    /// price at all.
    ///
    /// All four fields are in dollars per million tokens.
    pub fn explicit(input: f64, output: f64, cache_read: f64, cache_write: f64) -> Self {
        ModelRate {
            input: Usd::from_dollars(input),
            output: Usd::from_dollars(output),
            cache_read: Usd::from_dollars(cache_read),
            cache_write: Usd::from_dollars(cache_write),
        }
    }

    /// What `usage` costs at this rate.
    pub fn cost(&self, usage: TokenUsage) -> Usd {
        self.input.per_million(usage.input_tokens)
            + self.output.per_million(usage.output_tokens)
            + self.cache_read.per_million(usage.cache_read_tokens)
            + self.cache_write.per_million(usage.cache_creation_tokens)
    }
}

/// Where a rate came from — the provenance a number on a spend page owes its
/// reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RateSource {
    /// The dated table compiled into this binary.
    Builtin,
    /// `[defaults.rates."<model>"]` in `routing.toml`, which the table cannot
    /// know about: a negotiated rate, or a model shipped after this binary.
    Config,
}

impl RateSource {
    pub fn as_str(self) -> &'static str {
        match self {
            RateSource::Builtin => "built-in table",
            RateSource::Config => "routing.toml",
        }
    }
}

/// A rate, the key it was found under, and where that came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateMatch {
    /// The table key that matched — often shorter than the model the run
    /// reported, because a dated model id (`claude-opus-5-20260101`) prices off
    /// its family. Shown so a reader can tell an exact hit from a family one.
    pub key: String,
    pub rate: ModelRate,
    pub source: RateSource,
}

/// The rates a board prices with: a snapshot, its date, and whatever the
/// operator overrode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateTable {
    /// `YYYY-MM-DD` — when the built-in half of this table was last checked
    /// against published pricing. Carried onto every priced total, because a
    /// figure whose rates might be a year old is a different claim from one
    /// priced this morning.
    pub as_of: String,
    /// Model id → rate, lowercase keys.
    pub entries: BTreeMap<String, ModelRate>,
    /// Which of those keys came from config rather than the built-in table.
    pub overridden: Vec<String>,
}

/// When the built-in table was last checked against published pricing.
///
/// Bump this **only** with the rates it describes. A date moved on its own is
/// the failure mode this whole design is about: a stale number wearing a fresh
/// timestamp.
pub const BUILTIN_AS_OF: &str = "2026-08-12";

/// Past this, `doctor` says the table is old. Rates change a few times a year,
/// so a table older than two quarters has probably missed one.
pub const STALE_AFTER_DAYS: i64 = 180;

/// The shipped table: published API list prices, per million tokens, as of
/// [`BUILTIN_AS_OF`].
///
/// Carries the models this board dispatches: Anthropic (Claude), OpenAI
/// (Codex), and DeepSeek (via opencode). Gemini has no harness yet (gh#426);
/// the row will land when it does.
///
/// Dated ids (`claude-haiku-4-5-20251001`) are not listed: [`RateTable::rate_for`]
/// falls back to the longest matching family prefix, so one row prices every
/// snapshot of its model.
pub fn builtin() -> RateTable {
    // (id, input $/MTok, output $/MTok). Cache rates derive from input at the
    // published Anthropic multipliers (0.1× read, 1.25× write) — see
    // [`ModelRate::published`].
    const ROWS: &[(&str, f64, f64)] = &[
        ("claude-fable-5", 10.0, 50.0),
        ("claude-mythos-5", 10.0, 50.0),
        ("claude-opus-5", 5.0, 25.0),
        ("claude-opus-4-8", 5.0, 25.0),
        ("claude-opus-4-7", 5.0, 25.0),
        ("claude-opus-4-6", 5.0, 25.0),
        ("claude-opus-4-5", 5.0, 25.0),
        ("claude-opus-4-1", 15.0, 75.0),
        ("claude-opus-4-0", 15.0, 75.0),
        // Sonnet 5 ran an introductory $2/$10 into 2026-08-31; the list price
        // is what this table carries, and an operator on the intro rate says so
        // with an override rather than by this file guessing today's date.
        ("claude-sonnet-5", 3.0, 15.0),
        ("claude-sonnet-4-6", 3.0, 15.0),
        ("claude-sonnet-4-5", 3.0, 15.0),
        ("claude-sonnet-4-0", 3.0, 15.0),
        ("claude-haiku-4-5", 1.0, 5.0),
        // OpenAI / Codex — short-context standard pricing (platform.openai.com/docs/pricing).
        // Cache rates match the Anthropic multipliers exactly.
        ("gpt-5.6-sol", 5.0, 30.0),
        ("gpt-5.6-terra", 2.0, 12.0),
    ];
    let mut entries: BTreeMap<String, ModelRate> = ROWS
        .iter()
        .map(|(id, input, output)| ((*id).to_string(), ModelRate::published(*input, *output)))
        .collect();
    // DeepSeek V4 Flash — cache rates are published explicitly (api-docs.deepseek.com).
    // A cache read is 0.02× input ($0.0028/M), not the Anthropic 0.1×. No
    // separate cache-write price is published; writes are the same as fresh
    // input.
    entries.insert(
        "deepseek-v4-flash".to_string(),
        ModelRate::explicit(0.14, 0.28, 0.0028, 0.14),
    );
    RateTable {
        as_of: BUILTIN_AS_OF.to_string(),
        entries,
        overridden: Vec::new(),
    }
}

impl RateTable {
    /// A board that has been told to price nothing — no built-in table, no
    /// overrides. Not the same as an empty board: this is the state the page
    /// renders as "rates not configured" rather than as $0.
    pub fn empty(as_of: impl Into<String>) -> Self {
        RateTable {
            as_of: as_of.into(),
            entries: BTreeMap::new(),
            overridden: Vec::new(),
        }
    }

    /// Apply an operator's overrides, replacing built-in rows and adding rows
    /// the table never had.
    pub fn with_overrides(
        mut self,
        overrides: impl IntoIterator<Item = (String, ModelRate)>,
    ) -> Self {
        for (model, rate) in overrides {
            let key = normalize(&model);
            if key.is_empty() {
                continue;
            }
            self.entries.insert(key.clone(), rate);
            if !self.overridden.contains(&key) {
                self.overridden.push(key);
            }
        }
        self.overridden.sort();
        self
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The rate for a model the run reported, or `None` — which is *unpriced*,
    /// and never a zero.
    ///
    /// Matching, in order: the normalized id exactly, then the longest table
    /// key it starts with. The prefix step is what makes one row price every
    /// dated snapshot of a model, and longest-wins is what keeps
    /// `claude-opus-4-8` off `claude-opus-4`'s rate if both are ever listed.
    pub fn rate_for(&self, model: &str) -> Option<RateMatch> {
        let id = normalize(model);
        if id.is_empty() {
            return None;
        }
        let (key, rate) = match self.entries.get_key_value(&id) {
            Some((key, rate)) => (key, rate),
            None => self
                .entries
                .iter()
                .filter(|(key, _)| id.starts_with(key.as_str()))
                .max_by_key(|(key, _)| key.len())?,
        };
        Some(RateMatch {
            key: key.clone(),
            rate: *rate,
            source: if self.overridden.contains(key) {
                RateSource::Config
            } else {
                RateSource::Builtin
            },
        })
    }

    /// How old the table is on `today` (`YYYY-MM-DD`), in days. `None` when
    /// either date is unparseable — which reads as "cannot say", not as fresh.
    pub fn age_days(&self, today: &str) -> Option<i64> {
        let parse = |s: &str| chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok();
        let (as_of, now) = (parse(&self.as_of)?, parse(today)?);
        Some((now - as_of).num_days())
    }

    /// Is the table old enough to say so out loud? `None` days is not stale —
    /// see [`RateTable::age_days`].
    pub fn is_stale(&self, today: &str) -> bool {
        self.age_days(today).is_some_and(|d| d > STALE_AFTER_DAYS)
    }

    /// One line naming what these rates are and how old: what `doctor` prints
    /// and what a spend page puts under its headline.
    pub fn provenance(&self) -> String {
        let builtin = self.entries.len() - self.overridden.len();
        match (builtin, self.overridden.len()) {
            (0, 0) => "no rates configured".to_string(),
            (0, n) => format!("{n} model rate(s) from routing.toml"),
            (b, 0) => format!("{b} model rate(s), list prices as of {}", self.as_of),
            (b, n) => format!(
                "{b} model rate(s), list prices as of {}, plus {n} overridden in routing.toml",
                self.as_of
            ),
        }
    }
}

/// A model id as the table keys it: trimmed, lowercased, and with the vendor
/// prefixes a gateway bolts on removed.
///
/// A run that reports `us.anthropic.claude-opus-5` is running the model this
/// table prices, and a board that priced one spelling and not the other would
/// show half its spend as unpriced for no reason a reader could act on.
fn normalize(model: &str) -> String {
    let mut id = model.trim().to_ascii_lowercase();
    for prefix in [
        "us.anthropic.",
        "eu.anthropic.",
        "apac.anthropic.",
        "anthropic.",
        "anthropic/",
        "openai/",
        "deepseek/",
        "bedrock/",
        "vertex/",
        "models/",
    ] {
        if let Some(rest) = id.strip_prefix(prefix) {
            id = rest.to_string();
        }
    }
    // A Vertex-style `claude-opus-4-5@20251101` names the same model as the
    // dated form; the family prefix match handles the rest.
    if let Some((head, _)) = id.split_once('@') {
        id = head.to_string();
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64, read: u64, write: u64) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: read,
            cache_creation_tokens: write,
        }
    }

    /// The whole reason this module exists: a coding agent's spend is mostly
    /// cache reads, and pricing them as input would be wrong by a multiple
    /// rather than by a rounding error.
    #[test]
    fn the_four_buckets_are_priced_apart_and_it_is_not_a_rounding_error() {
        let rate = ModelRate::published(5.0, 25.0);
        // A turn of a long-context agent: a little fresh input, a lot replayed.
        let turn = usage(10_000, 2_000, 1_000_000, 20_000);
        let real = rate.cost(turn);
        // $0.05 + $0.05 + $0.50 + $0.125
        assert_eq!(real, Usd::from_dollars(0.725));

        // The same turn priced as if every input token were fresh input.
        let flat = rate.input.per_million(turn.input_total())
            + rate.output.per_million(turn.output_tokens);
        assert_eq!(flat, Usd::from_dollars(5.20));
        assert!(
            flat.micros > real.micros * 5,
            "one rate per model would be wrong by a multiple: {flat:?} vs {real:?}"
        );
    }

    #[test]
    fn a_rate_is_per_million_tokens_and_rounds_once() {
        let rate = ModelRate::published(3.0, 15.0);
        assert_eq!(rate.cache_read, Usd::from_dollars(0.30));
        assert_eq!(rate.cache_write, Usd::from_dollars(3.75));
        assert_eq!(rate.input.per_million(1_000_000), Usd::from_dollars(3.0));
        assert_eq!(rate.input.per_million(0), Usd::ZERO);
        // A single token is a real, tiny amount rather than a zero.
        assert_eq!(rate.input.per_million(1).micros, 3);
    }

    #[test]
    fn money_survives_the_wire_as_dollars() {
        let amount = Usd::from_dollars(4.283_915);
        let json = serde_json::to_value(amount).expect("serializes");
        // Dollars on the wire, so `stats --json` is checkable by eye.
        assert!((json.as_f64().expect("a number") - 4.283_915).abs() < 1e-9);
        let back: Usd = serde_json::from_value(json).expect("deserializes");
        assert_eq!(back, amount);
        // And a whole board's worth is still exact.
        let big = Usd::from_dollars(1_234_567.89);
        assert_eq!(
            serde_json::from_value::<Usd>(serde_json::to_value(big).unwrap()).unwrap(),
            big
        );
    }

    #[test]
    fn amounts_read_at_the_scale_they_matter_at() {
        assert_eq!(human_usd(Usd::ZERO), "$0.00");
        // Under a cent keeps its digits: `$0.00` on a real amount is the one
        // rendering that says "free" about work that was not.
        assert_eq!(human_usd(Usd::from_dollars(0.0042)), "$0.0042");
        assert_eq!(human_usd(Usd::from_dollars(0.14)), "$0.14");
        assert_eq!(human_usd(Usd::from_dollars(1.316)), "$1.32");
        assert_eq!(human_usd(Usd::from_dollars(248.4)), "$248");
    }

    #[test]
    fn a_dated_model_prices_off_its_family_and_a_gateway_prefix_is_not_a_new_model() {
        let table = builtin();
        let opus = table.rate_for("claude-opus-5").expect("listed");
        assert_eq!(opus.key, "claude-opus-5");
        assert_eq!(opus.source, RateSource::Builtin);

        for spelling in [
            "claude-opus-5-20260101",
            "us.anthropic.claude-opus-5",
            "anthropic/claude-opus-5",
            "  Claude-Opus-5  ",
        ] {
            let m = table
                .rate_for(spelling)
                .unwrap_or_else(|| panic!("{spelling}"));
            assert_eq!(m.rate, opus.rate, "{spelling}");
        }
        // Longest prefix wins, so a newer sibling never inherits an older one's
        // rate.
        assert_eq!(
            table.rate_for("claude-opus-4-8").unwrap().key,
            "claude-opus-4-8"
        );
    }

    /// The rule the rest of the feature is built on: a model nobody priced is
    /// unpriced, and a caller cannot mistake that for free.
    #[test]
    fn a_model_the_table_never_heard_of_is_unpriced_rather_than_free() {
        let table = builtin();
        assert!(table.rate_for("gpt-5.6-luna").is_none());
        assert!(table.rate_for("unnamed model").is_none());
        assert!(table.rate_for("").is_none());
        assert!(table.rate_for("   ").is_none());
    }

    #[test]
    fn an_override_replaces_a_rate_and_says_where_it_came_from() {
        let table = builtin().with_overrides([
            // A negotiated rate no lookup would ever know.
            ("claude-opus-5".to_string(), ModelRate::published(4.0, 20.0)),
            // A different price for a model the table does ship.
            (
                "gpt-5.6-terra".to_string(),
                ModelRate::published(1.25, 10.0),
            ),
        ]);
        let opus = table.rate_for("claude-opus-5").expect("overridden");
        assert_eq!(opus.rate.input, Usd::from_dollars(4.0));
        assert_eq!(opus.source, RateSource::Config);
        let gpt = table.rate_for("gpt-5.6-terra").expect("added");
        assert_eq!(gpt.source, RateSource::Config);
        // The built-in rows are still built-in.
        assert_eq!(
            table.rate_for("claude-haiku-4-5").unwrap().source,
            RateSource::Builtin
        );
        assert!(table.provenance().contains("2 overridden"));
    }

    #[test]
    fn codex_and_deepseek_models_are_in_the_shipped_table() {
        let table = builtin();
        let sol = table.rate_for("gpt-5.6-sol").expect("sol is priced");
        assert_eq!(sol.key, "gpt-5.6-sol");
        assert_eq!(sol.rate.input, Usd::from_dollars(5.0));
        assert_eq!(sol.rate.output, Usd::from_dollars(30.0));
        assert_eq!(sol.rate.cache_read, Usd::from_dollars(0.50));
        assert_eq!(sol.rate.cache_write, Usd::from_dollars(6.25));
        assert_eq!(sol.source, RateSource::Builtin);

        let terra = table.rate_for("gpt-5.6-terra").expect("terra is priced");
        assert_eq!(terra.rate.input, Usd::from_dollars(2.0));
        assert_eq!(terra.rate.output, Usd::from_dollars(12.0));

        // DeepSeek V4 Flash — explicit cache rates, not derived.
        let ds = table.rate_for("deepseek-v4-flash").expect("deepseek is priced");
        assert_eq!(ds.rate.input, Usd::from_dollars(0.14));
        assert_eq!(ds.rate.output, Usd::from_dollars(0.28));
        assert_eq!(ds.rate.cache_read, Usd::from_dollars(0.0028));
        assert_eq!(ds.rate.cache_write, Usd::from_dollars(0.14));
        assert_eq!(ds.source, RateSource::Builtin);

        // Cost rounds correctly: 1M input from cache is $0.0028, not $0.014.
        let turn = TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        };
        assert_eq!(ds.rate.cost(turn), Usd::from_dollars(0.14));
        let cached = TokenUsage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 1_000_000,
            cache_creation_tokens: 0,
        };
        assert_eq!(ds.rate.cost(cached), Usd::from_dollars(0.0028));
    }

    #[test]
    fn deepseek_provider_prefix_is_stripped_during_normalization() {
        let table = builtin();
        let direct = table.rate_for("deepseek-v4-flash").unwrap();
        for spelling in [
            "deepseek/deepseek-v4-flash",
            "DeepSeek/DeepSeek-V4-Flash",
            "  deepseek/deepseek-v4-flash  ",
        ] {
            let m = table
                .rate_for(spelling)
                .unwrap_or_else(|| panic!("{spelling}"));
            assert_eq!(m.rate, direct.rate, "{spelling}");
        }
    }

    #[test]
    fn the_table_says_how_old_it_is_and_never_implies_freshness() {
        let table = builtin();
        assert_eq!(table.as_of, BUILTIN_AS_OF);
        assert_eq!(table.age_days("2026-08-12"), Some(0));
        assert_eq!(table.age_days("2026-08-24"), Some(12));
        assert!(!table.is_stale("2026-08-24"));
        assert!(table.is_stale("2027-08-12"));
        // A date it cannot read is "cannot say", which is not the same as fresh
        // — and must not read as stale either.
        assert_eq!(table.age_days("sometime"), None);
        assert!(!table.is_stale("sometime"));
        assert!(table.provenance().contains(BUILTIN_AS_OF));
    }

    #[test]
    fn a_board_with_no_rates_at_all_says_so() {
        let empty = RateTable::empty(BUILTIN_AS_OF);
        assert!(empty.is_empty());
        assert_eq!(empty.rate_for("claude-opus-5"), None);
        assert_eq!(empty.provenance(), "no rates configured");
    }
}
