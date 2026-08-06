//! The billing guard (gh#101): never spend somebody's subscription silently.
//!
//! gh#59 made *which* account a dispatch spends explicit, and gh#74 made every
//! frontend send one and record who pressed enter. What neither did was say
//! anything about the case those two made visible: a dispatch that names no
//! account runs on the box's own CLI login, which on a shared box is the box
//! owner's subscription, whoever released the work. The teammate does not know
//! they are spending it; the owner finds out on their usage page.
//!
//! This module is the policy half. The *words* — "bills brede@tally.no", the
//! CLI line, the comment suffix — live in [`comet_proto::view::board`] beside
//! the other derivations both viewports share, because four surfaces say them.
//! What lives here is the mode ([`GuardMode`]) and the one refusal it can
//! produce ([`guard`]).
//!
//! ## Honesty
//!
//! The comparison is *claim versus slot email*: the `viaUser` a frontend sent
//! against the email on the agent-account the run will spend. A relayed board
//! call arrives as the device room's owner (docs/BOARD.md §H9), so the box
//! cannot check that claim — a frontend willing to lie about who is signed in
//! walks straight through `require-own`. This is a seatbelt, not a lock, and it
//! stays one until #66's verified identity lands. It is worth having anyway:
//! the failure it exists for is nobody noticing, not somebody attacking.

use comet_proto::HarnessId;
use comet_proto::view::board::{bills_warning, cross_billed};

/// What the board does about a dispatch that spends somebody else's
/// subscription — `[defaults] billing_guard`, overridable per route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GuardMode {
    /// Say so, everywhere, and release anyway. The default: the board's job
    /// here is to make the spend visible to both parties, and a box where two
    /// people deliberately share one subscription is a normal box.
    #[default]
    Warn,
    /// Refuse a cross-billed dispatch unless it names the owner outright.
    RequireOwn,
    /// Say nothing. A real choice on a single-person box, where every dispatch
    /// is cross-billed against nobody and the warning is noise — which is why
    /// `doctor` reports it as a choice rather than as something unconfigured.
    Off,
}

impl GuardMode {
    pub fn as_str(self) -> &'static str {
        match self {
            GuardMode::Warn => "warn",
            GuardMode::RequireOwn => "require-own",
            GuardMode::Off => "off",
        }
    }

    /// Does this mode say anything at all about a cross-billed run?
    pub fn speaks(self) -> bool {
        !matches!(self, GuardMode::Off)
    }
}

/// The spellings `billing_guard` accepts, for error messages and `doctor`.
pub const GUARD_MODES: &[&str] = &["warn", "require-own", "off"];

/// Parse a `billing_guard` value.
///
/// An unrecognised value is an `Err` rather than a silent fallback to `warn`,
/// for the reason `max_duration`'s parse is: `billing_gaurd = "require-own"`
/// and no key at all look identical on the board, and only one of them is what
/// somebody meant. `RoutingConfig::validate` refuses the config instead.
pub fn parse_guard_mode(s: &str) -> Result<GuardMode, String> {
    match s.trim().to_ascii_lowercase().replace('_', "-").as_str() {
        "warn" => Ok(GuardMode::Warn),
        "require-own" | "requireown" => Ok(GuardMode::RequireOwn),
        "off" | "none" | "false" => Ok(GuardMode::Off),
        _ => Err(format!(
            "`{s}` is not a billing guard mode; write one of: {}",
            GUARD_MODES.join(", ")
        )),
    }
}

/// Who a dispatch bills, resolved at release time (docs/BOARD.md §H11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Billing {
    /// The email whose subscription the run spends: the named slot's login, or
    /// — when the dispatch names no slot — the box's own, displayed as the
    /// operator's. `None` when the device cannot name one, which accuses
    /// nobody.
    pub billed_to: Option<String>,
    /// Who the dispatching frontend said is signed in there (`viaUser`). A
    /// claim; see the module docs.
    pub dispatcher: Option<String>,
    /// The harness the run spends it on — a Claude slot and a Codex slot are
    /// different subscriptions, and the warning names which.
    pub harness: HarnessId,
}

impl Billing {
    /// Is this run spending somebody else's subscription?
    pub fn cross_billed(&self) -> bool {
        cross_billed(self.billed_to.as_deref(), self.dispatcher.as_deref())
    }

    /// The one line every surface leads with, or `None` when there is nothing
    /// to say.
    pub fn warning(&self) -> Option<String> {
        let billed = self.billed_to.as_deref()?;
        self.cross_billed()
            .then(|| bills_warning(billed, self.harness))
    }

    /// What the upstream dispatch comment appends, so the record is public to
    /// both parties rather than living on one usage page. Empty when the run
    /// bills its own releaser — a comment that names the payer on every
    /// dispatch says nothing on the one that matters.
    pub fn comment_suffix(&self) -> String {
        match (self.cross_billed(), self.billed_to.as_deref()) {
            (true, Some(billed)) => comet_proto::view::board::bills_comment_suffix(billed),
            _ => String::new(),
        }
    }
}

/// The refusal `require-own` owes a cross-billed dispatch, or `Ok(())`.
///
/// `acknowledged` is the explicit override: `--bill <slot>` on the CLI, the
/// confirm dialog's second press in a frontend. It has to *name* the payer —
/// the slot id the dispatch will spend, or that slot's email — because the
/// whole point of the mode is that spending somebody else's plan should be a
/// sentence you typed, not a flag you set once and forgot.
///
/// Called beside the concurrency cap, before any attempt row exists: a refusal
/// that leaves a `failed` attempt behind has already cost the operator the
/// cleanup this mode exists to avoid.
pub fn guard(mode: GuardMode, billing: &Billing, acknowledged: bool) -> anyhow::Result<()> {
    let Some(warning) = billing.warning() else {
        return Ok(());
    };
    if mode != GuardMode::RequireOwn || acknowledged {
        return Ok(());
    }
    anyhow::bail!(
        "{warning}. `[defaults] {}` refuses this; pass `--bill {}` if you mean to spend it",
        comet_proto::view::board::REQUIRE_OWN_REFUSAL,
        billing.billed_to.as_deref().unwrap_or_default()
    )
}

/// Does an acknowledgement name the account this dispatch will actually spend?
///
/// Either spelling counts: the agent-account slot id (what `--bill` normally
/// carries, and what also selects the account) or the email on it (the only
/// thing there is to name when the run is on the box's own login and has no
/// slot id at all). A value that names *something else* is not an
/// acknowledgement of this — it is a typo, and letting it through would spend
/// the account it did not name.
pub fn acknowledges(ack: Option<&str>, slot: Option<&str>, billed_to: Option<&str>) -> bool {
    let Some(ack) = ack.map(str::trim).filter(|a| !a.is_empty()) else {
        return false;
    };
    [slot, billed_to]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .any(|v| v.eq_ignore_ascii_case(ack))
}

/// Is this `--bill` value a slot id rather than an email?
///
/// `--bill` does double duty — it names the payer *and* selects the account —
/// so the board has to tell the two spellings apart. It cannot validate a slot
/// id (which logins a device has saved is engine knowledge), and it does not
/// need to: an `@` is an email, everything else is an id, and an id that names
/// no saved login fails the dispatch with the engine's own message.
pub fn bill_names_a_slot(bill: &str) -> bool {
    !bill.contains('@')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn billing(billed: Option<&str>, by: Option<&str>) -> Billing {
        Billing {
            billed_to: billed.map(str::to_string),
            dispatcher: by.map(str::to_string),
            harness: HarnessId::ClaudeCode,
        }
    }

    #[test]
    fn modes_parse_by_the_spellings_the_docs_use_and_refuse_the_rest() {
        assert_eq!(parse_guard_mode("warn"), Ok(GuardMode::Warn));
        assert_eq!(parse_guard_mode("require-own"), Ok(GuardMode::RequireOwn));
        assert_eq!(parse_guard_mode("require_own"), Ok(GuardMode::RequireOwn));
        assert_eq!(parse_guard_mode("  OFF "), Ok(GuardMode::Off));
        let err = parse_guard_mode("require-mine").unwrap_err();
        assert!(err.contains("require-own"), "{err}");
        assert_eq!(GuardMode::default(), GuardMode::Warn);
    }

    /// The whole of `warn`: it says something exactly when somebody else pays.
    #[test]
    fn the_warning_fires_only_on_a_run_that_charges_somebody_else() {
        let cross = billing(Some("brede@tally.no"), Some("ana@example.com"));
        assert_eq!(
            cross.warning().as_deref(),
            Some("this run bills brede@tally.no's Claude — pass --account <your slot>")
        );
        assert_eq!(
            cross.comment_suffix(),
            " · on brede@tally.no's subscription"
        );

        // Their own subscription, an unattributed dispatch, and a box that
        // cannot name the login all say nothing.
        for quiet in [
            billing(Some("brede@tally.no"), Some("brede@tally.no")),
            billing(Some("brede@tally.no"), None),
            billing(None, Some("ana@example.com")),
        ] {
            assert_eq!(quiet.warning(), None);
            assert_eq!(quiet.comment_suffix(), "");
        }
    }

    /// `warn` and `off` release; `require-own` refuses, and says both what it
    /// refused and how to mean it.
    #[test]
    fn require_own_is_the_only_mode_that_refuses() {
        let cross = billing(Some("brede@tally.no"), Some("ana@example.com"));
        assert!(guard(GuardMode::Warn, &cross, false).is_ok());
        assert!(guard(GuardMode::Off, &cross, false).is_ok());

        let err = guard(GuardMode::RequireOwn, &cross, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("bills brede@tally.no's Claude"), "{err}");
        assert!(err.contains("--bill brede@tally.no"), "{err}");

        // Named outright, it goes through.
        assert!(guard(GuardMode::RequireOwn, &cross, true).is_ok());
        // And a run on your own account is never refused by any mode.
        let own = billing(Some("ana@example.com"), Some("ana@example.com"));
        assert!(guard(GuardMode::RequireOwn, &own, false).is_ok());
    }

    #[test]
    fn an_acknowledgement_has_to_name_the_account_it_is_acknowledging() {
        assert!(acknowledges(
            Some("slot-box"),
            Some("slot-box"),
            Some("brede@tally.no")
        ));
        // The box's own login has no slot id to name — the email is the only
        // spelling there is.
        assert!(acknowledges(
            Some("brede@tally.no"),
            None,
            Some("brede@tally.no")
        ));
        assert!(acknowledges(
            Some("BREDE@Tally.no"),
            None,
            Some("brede@tally.no")
        ));
        // Naming something else is a typo, not consent.
        assert!(!acknowledges(
            Some("slot-ana"),
            Some("slot-box"),
            Some("brede@tally.no")
        ));
        assert!(!acknowledges(
            None,
            Some("slot-box"),
            Some("brede@tally.no")
        ));
        assert!(!acknowledges(Some("  "), Some("slot-box"), None));
    }

    #[test]
    fn bill_tells_a_slot_id_from_an_email() {
        assert!(bill_names_a_slot("8f2c1d0a7b6e4539"));
        assert!(!bill_names_a_slot("brede@tally.no"));
    }
}
