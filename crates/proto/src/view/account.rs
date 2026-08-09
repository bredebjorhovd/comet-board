//! Who is signed in, and on what plan (gh#230) — the account footer's four
//! strings, derived from the one identity the engine reports.
//!
//! The board sweeps hosts, so the window in front of you can be showing a
//! device you are not sitting at, and the pages that price work
//! ([`super::stats`], [`super::rates`]) are only meaningful against an
//! account. A footer that says "Brede Bjørhovd / Alpha" is the cheapest way to
//! stop both being ambiguous — but only if it is *derived*. A literal in a
//! render function is worse than nothing: it kept saying `Alpha` to a signed-
//! out window, which is exactly the ambiguity the block exists to remove.
//!
//! So everything here is a function of [`AuthState`], including the signed-out
//! row: there is no state in which the footer has nothing to say, and no
//! string it can show that the identity did not decide.

use crate::AuthState;

/// The one tier comet ships while it is in alpha. Every account with a session
/// is on it.
///
/// This is the single place the tier is decided. Nothing about the identity
/// the engine reports carries an entitlement today — WorkOS gives the app a
/// user and an organization, and the edge mints tokens from those; no producer
/// in the stack has a plan to send. When one does, [`account_footer`] reads it
/// here and every surface follows, instead of each render site guessing again.
pub const PLAN_ALPHA: &str = "Alpha";

/// The plan line for a session that authenticated but has not picked a
/// workspace yet — the org gate's state, and not a tier.
pub const PLAN_NO_WORKSPACE: &str = "No workspace";

/// What the signed-out block says instead of going blank.
pub const SIGNED_OUT_NAME: &str = "Not signed in";
pub const SIGNED_OUT_PLAN: &str = "Sign in to sync";

/// Fallback avatar mark for a name with no letter or digit to take one from.
const NO_INITIAL: &str = "?";

/// How far the footer's identity got. Drives the one thing the block's menu
/// changes: whether it offers a way in or a way out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountState {
    /// No session — the block invites one.
    SignedOut,
    /// A session with no organization ([`AuthState::NeedsOrganization`]): a
    /// real account, so the menu still signs out, but not a tier.
    NeedsWorkspace,
    SignedIn,
}

impl AccountState {
    /// True when there is a session to end — the menu's Sign out / Sign in
    /// fork, and nothing else.
    pub fn has_session(self) -> bool {
        !matches!(self, AccountState::SignedOut)
    }
}

/// The account block at the foot of the sidebar, as strings ready to draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountFooter {
    /// Display name — the profile name when there is one, the email otherwise,
    /// [`SIGNED_OUT_NAME`] when there is no session.
    pub name: String,
    /// The avatar's single uppercase mark, taken from [`Self::name`].
    pub initial: String,
    /// The plan tier line under the name.
    pub plan: String,
    /// The address, for the menu's own line. `None` when signed out.
    pub email: Option<String>,
    pub state: AccountState,
}

/// Derive the footer from the engine's last `AuthStatus`.
///
/// `None` — no frame yet — reads signed out, which is what it is: the engine
/// reports `SignedOut` on connect, and until it does the app has no session.
pub fn account_footer(auth: Option<&AuthState>) -> AccountFooter {
    let signed_out = || AccountFooter {
        name: SIGNED_OUT_NAME.to_string(),
        initial: NO_INITIAL.to_string(),
        plan: SIGNED_OUT_PLAN.to_string(),
        email: None,
        state: AccountState::SignedOut,
    };
    let (user, state) = match auth {
        Some(AuthState::SignedIn { user, .. }) => (user, AccountState::SignedIn),
        Some(AuthState::NeedsOrganization { user }) => (user, AccountState::NeedsWorkspace),
        Some(AuthState::SignedOut) | None => return signed_out(),
    };
    // Same rule the terminal app's user row uses: a blank `name` is not a name.
    let name = user
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or(user.email.as_str())
        .to_string();
    // An account with neither is a wire shape we should still draw rather than
    // an empty row.
    let name = if name.is_empty() {
        SIGNED_OUT_NAME.to_string()
    } else {
        name
    };
    AccountFooter {
        initial: initial_of(&name),
        plan: match state {
            AccountState::SignedIn => PLAN_ALPHA.to_string(),
            _ => PLAN_NO_WORKSPACE.to_string(),
        },
        email: (!user.email.is_empty()).then(|| user.email.clone()),
        name,
        state,
    }
}

/// The avatar mark: the first letter or digit of the name, uppercased.
///
/// Skipping the non-alphanumerics matters for the email fallback — a `+`-tagged
/// or quoted address should not put punctuation in the circle.
fn initial_of(name: &str) -> String {
    name.chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| NO_INITIAL.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UserProfile;

    fn user(name: Option<&str>, email: &str) -> UserProfile {
        UserProfile {
            id: "u1".into(),
            email: email.into(),
            name: name.map(str::to_string),
        }
    }

    #[test]
    fn a_signed_in_account_shows_its_name_and_its_tier() {
        let auth = AuthState::SignedIn {
            user: user(Some("Brede Bjørhovd"), "brede@tally.no"),
            org_id: Some("org_1".into()),
        };
        let footer = account_footer(Some(&auth));
        assert_eq!(footer.name, "Brede Bjørhovd");
        assert_eq!(footer.initial, "B");
        assert_eq!(footer.plan, PLAN_ALPHA);
        assert_eq!(footer.email.as_deref(), Some("brede@tally.no"));
        assert_eq!(footer.state, AccountState::SignedIn);
        assert!(footer.state.has_session());
    }

    #[test]
    fn a_nameless_account_falls_back_to_its_address() {
        let auth = AuthState::SignedIn {
            user: user(None, "brede@tally.no"),
            org_id: None,
        };
        let footer = account_footer(Some(&auth));
        assert_eq!(footer.name, "brede@tally.no");
        assert_eq!(footer.initial, "B");
        // A blank name is the same thing as no name.
        let blank = AuthState::SignedIn {
            user: user(Some("   "), "brede@tally.no"),
            org_id: None,
        };
        assert_eq!(account_footer(Some(&blank)).name, "brede@tally.no");
    }

    #[test]
    fn signed_out_degrades_to_a_row_rather_than_a_hole() {
        for auth in [Some(&AuthState::SignedOut), None] {
            let footer = account_footer(auth);
            assert_eq!(footer.name, SIGNED_OUT_NAME);
            assert_eq!(footer.plan, SIGNED_OUT_PLAN);
            assert_eq!(footer.initial, NO_INITIAL);
            assert_eq!(footer.email, None);
            assert_eq!(footer.state, AccountState::SignedOut);
            assert!(!footer.state.has_session());
        }
    }

    /// The defect the derivation exists to prevent: a tier is a property of a
    /// session, so no window without one may claim to be on a plan.
    #[test]
    fn no_tier_is_claimed_without_a_session() {
        assert_ne!(account_footer(None).plan, PLAN_ALPHA);
        assert_ne!(account_footer(Some(&AuthState::SignedOut)).plan, PLAN_ALPHA);
        let pending = AuthState::NeedsOrganization {
            user: user(Some("Brede"), "brede@tally.no"),
        };
        let footer = account_footer(Some(&pending));
        assert_eq!(footer.plan, PLAN_NO_WORKSPACE);
        // …but it is still a real account: the menu signs it out, not in.
        assert_eq!(footer.state, AccountState::NeedsWorkspace);
        assert!(footer.state.has_session());
        assert_eq!(footer.name, "Brede");
    }

    #[test]
    fn the_avatar_mark_skips_punctuation_and_uppercases() {
        assert_eq!(initial_of("brede@tally.no"), "B");
        assert_eq!(initial_of("\"quoted\"@tally.no"), "Q");
        assert_eq!(initial_of("øystein"), "Ø");
        assert_eq!(initial_of("4chan"), "4");
        assert_eq!(initial_of("—"), NO_INITIAL);
    }
}
