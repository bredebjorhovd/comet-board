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
//! ## What the comparison is made of
//!
//! Whose subscription the run spends is [`Billing::billed_to`] — the email on
//! the agent-account slot, resolved by the engine. Who dispatched is
//! [`Attribution`], and gh#161 is the difference between its two loaded
//! variants: a **verified** dispatcher is the identity the edge Worker checked
//! before the frame reached the device room and the relay stamped onto the
//! frame itself (`edge/src/device-room.ts`, `u`/`o`); a **claimed** one is the
//! `viaUser` a frontend typed.
//!
//! `require-own` refuses on the first and not on the second, and the two never
//! render the same. Until gh#161 there was only the claim, so the mode was a
//! seatbelt by construction: a frontend willing to lie about who is signed in
//! walked straight through it. It is a lock over the relay now — a lie about
//! `viaUser` changes nothing about what gets spent — and it stays a seatbelt
//! for a dispatch issued *on the box itself*, which is correct and not a gap:
//! a call with no relay stamp came from the device's own IPC port, so whoever
//! made it is already the person the box runs as.

use comet_proto::HarnessId;
use comet_proto::view::board::{REQUIRE_OWN_REFUSAL, bills_warning, cross_billed};

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

/// Who released a dispatch, and how well the box knows it (gh#161).
///
/// The order is the strength order, and every surface that says a name owes
/// the reader the difference: "we know who this was" and "they told us" are
/// not the same sentence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Attribution {
    /// Nobody said and nothing was stamped: `comet-board` from a shell nobody
    /// signed into, an orchestrating agent, every attempt from before the
    /// frontends sent `viaUser` at all. Names nobody to have been wronged, so
    /// it is never refused.
    #[default]
    Nobody,
    /// The dispatching frontend's word, on a call that arrived with no relay
    /// stamp — which is to say from the box's own device. Unverified, and not
    /// suspicious for it: nothing else can reach the local IPC port.
    Claimed(String),
    /// The edge verified the caller and the box put an email to them. The only
    /// variant `require-own` will refuse *on*, and the only one it will let a
    /// cross-billed release through on.
    Verified(String),
    /// The edge verified somebody and the box could not name them: a teammate
    /// the workspace roster did not resolve, or a roster it could not read.
    /// Under `require-own` this refuses — see [`guard`].
    Unnamed {
        /// The verified WorkOS user id, which is what the refusal names.
        user_id: String,
        /// What their frontend claimed, kept for the record and never compared
        /// against anything.
        claimed: Option<String>,
    },
}

impl Attribution {
    /// The email to compare against the account being spent, or `None` when
    /// there is nothing the box is willing to compare.
    ///
    /// `Unnamed` answers `None` deliberately: it *has* a claim, and the claim
    /// is exactly what stopped being good enough the moment a verified identity
    /// arrived beside it.
    pub fn email(&self) -> Option<&str> {
        match self {
            Attribution::Verified(email) | Attribution::Claimed(email) => {
                Some(email.as_str()).filter(|e| !e.trim().is_empty())
            }
            Attribution::Nobody | Attribution::Unnamed { .. } => None,
        }
    }

    /// Did the edge verify this dispatcher *and* the box name them? What the
    /// attempt records, and what tells a reader which sentence they are
    /// reading.
    pub fn is_verified(&self) -> bool {
        matches!(self, Attribution::Verified(_))
    }

    /// What goes on the attempt row as "who released this": the verified email
    /// where there is one, the claim otherwise, and the raw user id when a
    /// verified caller left no claim — an id nobody can resolve still beats
    /// recording nothing.
    pub fn name(&self) -> Option<&str> {
        match self {
            Attribution::Verified(email) | Attribution::Claimed(email) => Some(email.as_str()),
            Attribution::Unnamed { user_id, claimed } => {
                Some(claimed.as_deref().unwrap_or(user_id.as_str()))
            }
            Attribution::Nobody => None,
        }
        .filter(|n| !n.trim().is_empty())
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
    /// Who released it, at whatever strength the transport allowed.
    pub dispatcher: Attribution,
    /// The harness the run spends it on — a Claude slot and a Codex slot are
    /// different subscriptions, and the warning names which.
    pub harness: HarnessId,
    /// Did anything name an agent-account slot — the dispatch's `--account` or
    /// the route's? `false` means the run falls to the box's own CLI login,
    /// which is the silent default gh#161 is also about: on a one-person box it
    /// is right, and on a shared one it spends the owner's plan for whoever
    /// pressed enter.
    pub named_slot: bool,
    /// Is the dispatcher the identity the box itself runs as? True for every
    /// dispatch issued on the box, which is why the unnamed-slot rule below
    /// cannot fire on a local one.
    pub is_owner: bool,
}

impl Billing {
    /// Is this run spending somebody else's subscription?
    pub fn cross_billed(&self) -> bool {
        cross_billed(self.billed_to.as_deref(), self.dispatcher.email())
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

/// The refusal `require-own` owes this dispatch, or `Ok(())`.
///
/// `acknowledged` is the explicit override: `--bill <slot>` on the CLI, the
/// confirm dialog's second press in a frontend. It has to *name* the payer —
/// the slot id the dispatch will spend, or that slot's email — because the
/// whole point of the mode is that spending somebody else's plan should be a
/// sentence you typed, not a flag you set once and forgot. It answers all three
/// refusals below, which is why it is checked first: each one is a way of
/// saying "somebody who is not you is paying for this", and naming the payer is
/// the one answer to that question.
///
/// Three grounds, in the order a reader wants them:
///
/// 1. **The run bills somebody else.** The old refusal, on the strength gh#161
///    gave it: a *verified* dispatcher whose email is not the account's.
/// 2. **The box cannot name the dispatcher.** A relayed call carries an
///    identity the edge verified, and the box could not resolve it to an
///    email — the roster was unreadable, or the person is not on it. Refusing
///    is the safe answer and the only one that stays true to the mode: the
///    alternative is falling back to the claim, and the claim is what
///    `require-own` stopped believing. It says the id out loud so the refusal
///    is actionable rather than mysterious.
/// 3. **Nothing named an account, and the dispatcher is not the box owner.**
///    A dispatch that names no slot spends the box's own CLI login. When the
///    box can name that login this is already case 1; when it cannot, case 1
///    goes quiet and a teammate's run charges the owner in silence, which is
///    the failure this mode exists for.
///
/// Two things are deliberately *not* grounds. A dispatch nobody attributed is
/// released: an unattributed release names no wronged party (an agent's
/// dispatch and a bare `comet-board` are both this). And a dispatch issued on
/// the box itself is never refused by case 2 or 3 — it carries no relay stamp
/// because it did not come through the relay, and treating the operator's own
/// shell as an unverifiable stranger would break every local board on the day
/// this shipped, for nothing.
///
/// Called beside the concurrency cap, before any attempt row exists: a refusal
/// that leaves a `failed` attempt behind has already cost the operator the
/// cleanup this mode exists to avoid.
pub fn guard(mode: GuardMode, billing: &Billing, acknowledged: bool) -> anyhow::Result<()> {
    if mode != GuardMode::RequireOwn || acknowledged {
        return Ok(());
    }
    let Some(refusal) = billing.refusal() else {
        return Ok(());
    };
    anyhow::bail!("{refusal}")
}

impl Billing {
    /// The sentence `require-own` refuses with, or `None` when it releases.
    ///
    /// Every branch carries [`REQUIRE_OWN_REFUSAL`] and a `--bill` that names
    /// something real: the panel tells this refusal from every other dispatch
    /// failure by that phrase and offers the confirm, and a refusal a frontend
    /// cannot act on is a dead end rather than a guard.
    fn refusal(&self) -> Option<String> {
        // 1. Somebody else's subscription, on a comparison the box stands behind.
        if let Some(warning) = self.warning() {
            return Some(format!(
                "{warning}. `[defaults] {REQUIRE_OWN_REFUSAL}` refuses this; pass `--bill {}` \
                 if you mean to spend it",
                self.billed_to.as_deref().unwrap_or_default()
            ));
        }
        // 2. A verified caller the box cannot put a name to.
        if let Attribution::Unnamed { user_id, .. } = &self.dispatcher {
            return Some(format!(
                "this dispatch came from {user_id}, and this box cannot tell whose account \
                 that is — `[defaults] {REQUIRE_OWN_REFUSAL}` will not spend a subscription \
                 for somebody it cannot name; pass `--bill <slot>` to say whose plan pays"
            ));
        }
        // 3. Nothing named an account, so the box's own login pays — the box
        //    cannot say whose login that is (or case 1 would have), and the
        //    dispatcher is not the person it belongs to.
        if !self.named_slot && !self.is_owner && self.billed_to.is_none() {
            return Some(format!(
                "this dispatch names no agent account, so it would run on this box's own \
                 login — `[defaults] {REQUIRE_OWN_REFUSAL}` refuses that for anyone but the \
                 box's owner; pass `--account <your slot>` to spend your own"
            ));
        }
        None
    }
}

/// Does an acknowledgement name the account this dispatch will actually spend?
///
/// Either spelling counts: the agent-account slot id (what `--bill` normally
/// carries, and what also selects the account) or the email on it (the only
/// thing there is to name when the run is on the box's own login and has no
/// slot id at all). A value that names *something else* is not an
/// acknowledgement of this — it is a typo, and letting it through would spend
/// the account it did not name.
///
/// The exception is the state where there is nothing to name at all: no slot,
/// and a box that could not read whose login its own is (gh#161's third
/// refusal). "Name the payer" cannot be asked of somebody the box has refused
/// to tell, so any non-empty value is the yes it can honestly ask for — and the
/// dispatch it acknowledges is exactly the one the box already said it could
/// not attribute.
pub fn acknowledges(ack: Option<&str>, slot: Option<&str>, billed_to: Option<&str>) -> bool {
    let Some(ack) = ack.map(str::trim).filter(|a| !a.is_empty()) else {
        return false;
    };
    let nameable: Vec<&str> = [slot, billed_to]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .collect();
    if nameable.is_empty() {
        return true;
    }
    nameable.iter().any(|v| v.eq_ignore_ascii_case(ack))
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

    /// A dispatch from the box's own device: the frontend's claim, an account
    /// named, nothing relayed. What every board did before gh#161.
    fn billing(billed: Option<&str>, by: Option<&str>) -> Billing {
        Billing {
            billed_to: billed.map(str::to_string),
            dispatcher: by
                .map(|b| Attribution::Claimed(b.to_string()))
                .unwrap_or_default(),
            harness: HarnessId::ClaudeCode,
            named_slot: true,
            is_owner: true,
        }
    }

    /// The same dispatch relayed from a teammate's laptop: no slot named (the
    /// silent default), and the box is not theirs.
    fn relayed(billed: Option<&str>, by: Attribution) -> Billing {
        Billing {
            billed_to: billed.map(str::to_string),
            dispatcher: by,
            harness: HarnessId::ClaudeCode,
            named_slot: false,
            is_owner: false,
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

    /// gh#161: the same run, dispatched by the same person, refused or
    /// released on which of the two identities the box got — and a lie about
    /// `viaUser` changing nothing either way.
    #[test]
    fn require_own_refuses_on_the_verified_dispatcher_and_not_on_the_claim() {
        let ana = "ana@example.com";
        let owner = "brede@tally.no";

        // Verified as somebody else: refused, naming what it would have spent.
        let verified = relayed(Some(owner), Attribution::Verified(ana.into()));
        let err = guard(GuardMode::RequireOwn, &verified, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("bills brede@tally.no's Claude"), "{err}");
        assert!(err.contains(REQUIRE_OWN_REFUSAL), "{err}");

        // What a lying `viaUser` would have to survive: the claim never
        // reaches this type at all. `DispatchOrigin::attribution` is where a
        // verified stamp discards it, and its own tests hold that line.
        assert_eq!(verified.dispatcher.email(), Some(ana));

        // Verified as the payer: released, cross-billing nobody.
        let own = relayed(Some(ana), Attribution::Verified(ana.into()));
        assert!(guard(GuardMode::RequireOwn, &own, false).is_ok());
        assert!(!own.cross_billed());
    }

    /// A verified caller the box cannot name is refused rather than trusted at
    /// its word — and the refusal says the id, so it can be acted on.
    #[test]
    fn require_own_refuses_a_dispatcher_it_cannot_put_a_name_to() {
        let unnamed = Attribution::Unnamed {
            user_id: "user_01J".into(),
            claimed: Some("brede@tally.no".into()),
        };
        let billing = relayed(Some("brede@tally.no"), unnamed.clone());
        let err = guard(GuardMode::RequireOwn, &billing, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("user_01J"), "{err}");
        assert!(err.contains(REQUIRE_OWN_REFUSAL), "{err}");
        // The claim it carried is not the comparison, and not the accusation.
        assert_eq!(unnamed.email(), None);
        assert!(!billing.cross_billed());
        assert!(!unnamed.is_verified());
        // But it is still the best record of who this was.
        assert_eq!(unnamed.name(), Some("brede@tally.no"));

        // Named outright, it goes through — and `warn`/`off` never refused.
        assert!(guard(GuardMode::RequireOwn, &billing, true).is_ok());
        assert!(guard(GuardMode::Warn, &billing, false).is_ok());
    }

    /// The silent default from the other side: a dispatch naming no slot spends
    /// the box's own login. When the box cannot even say whose that is, only
    /// the fact that the dispatcher is not its owner is left to go on.
    #[test]
    fn require_own_treats_an_unnamed_slot_as_cross_billing_unless_you_are_the_box() {
        let teammate = Attribution::Verified("ana@example.com".into());
        let nameless = relayed(None, teammate.clone());
        let err = guard(GuardMode::RequireOwn, &nameless, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("names no agent account"), "{err}");
        assert!(err.contains("--account <your slot>"), "{err}");

        // Naming one is the fix, and it releases even though the box still
        // cannot resolve the email.
        let named = Billing {
            named_slot: true,
            ..nameless.clone()
        };
        assert!(guard(GuardMode::RequireOwn, &named, false).is_ok());

        // So is being the box: the owner's own dispatch, and every dispatch
        // issued on the box itself, spends the box's login by definition.
        let mine = Billing {
            is_owner: true,
            ..nameless.clone()
        };
        assert!(guard(GuardMode::RequireOwn, &mine, false).is_ok());

        // And nothing here is a refusal under the other two modes.
        assert!(guard(GuardMode::Warn, &nameless, false).is_ok());
        assert!(guard(GuardMode::Off, &nameless, false).is_ok());
    }

    /// The local box is not collateral damage: with no relay stamp there is
    /// nothing to verify, and today's behaviour is today's behaviour.
    #[test]
    fn a_dispatch_on_the_box_itself_is_judged_exactly_as_before() {
        // Unattributed, and a box that cannot name its own login: released.
        let quiet = Billing {
            named_slot: false,
            ..billing(None, None)
        };
        assert!(guard(GuardMode::RequireOwn, &quiet, false).is_ok());
        assert_eq!(Attribution::Nobody.name(), None);

        // A claim that does not match the account still refuses — the seatbelt
        // it always was, on the one surface where it is still all there is.
        let cross = billing(Some("brede@tally.no"), Some("ana@example.com"));
        assert!(guard(GuardMode::RequireOwn, &cross, false).is_err());
        assert!(!cross.dispatcher.is_verified());
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
        // Nothing to name: no slot, and a box that could not read whose its own
        // login is. A yes is all there is to ask for (gh#161).
        assert!(acknowledges(Some("yes"), None, None));
        assert!(!acknowledges(None, None, None));
    }

    #[test]
    fn bill_tells_a_slot_id_from_an_email() {
        assert!(bill_names_a_slot("8f2c1d0a7b6e4539"));
        assert!(!bill_names_a_slot("brede@tally.no"));
    }
}
