//! Which harnesses *this* device can actually start (gh#187).
//!
//! `comet_board::runtime::runtime_options()` names the runtimes a dispatch may
//! be pointed at. That list is a constant — one entry per harness comet ships —
//! and it stays a constant, because which names are *spellable* is a property
//! of the board and not of any box. What the box adds is whether each of them
//! could start here:
//!
//! ```text
//! claude         /usr/local/bin/claude          ready
//! codex          /usr/local/bin/codex           ready
//! opencode       NOT INSTALLED                  not installed
//! cursor-agent   NOT INSTALLED                  no adapter in this build
//! ```
//!
//! Two independent facts, and both of them are cheap. Is the CLI on the disk
//! ([`comet_harness::locate_cli`] — the same resolution the adapter does at
//! spawn, minus the spawn), and does it have a credential
//! ([`AgentAccounts::signed_in`] — the same files the accounts page reads,
//! minus the network). Neither is worth caching; both are worth asking before
//! a dispatch cuts a worktree.
//!
//! Served three ways, all from here so they cannot disagree: `ListBoardRuntimes`
//! (which is relay-forwardable, so a laptop's picker asks the *box*), the
//! board's dispatch guard, and `comet-board doctor`.

use comet_proto::HarnessId;
use comet_proto::view::board::{RuntimeOption, RuntimeUnavailable};

use crate::agent_accounts::AgentAccounts;

/// Why `harness` could not start on this device, or `None` if it could.
///
/// `account` is the agent-account slot the dispatch would spend, when it names
/// one (gh#59). It suppresses the signed-out half deliberately: a run pointed
/// at a slot reads that slot's materialized config dir and never the CLI's own,
/// so the box's login being absent says nothing about it — and the slot itself
/// is checked, loudly, when the executor materializes it. A picker asking in
/// general passes `None` and gets the box's own login state, which is what a
/// run naming no slot would reach.
pub fn availability(
    accounts: &AgentAccounts,
    harness: HarnessId,
    account: Option<&str>,
) -> Option<RuntimeUnavailable> {
    match comet_harness::locate_cli(harness) {
        comet_harness::HarnessCli::Unsupported => return Some(RuntimeUnavailable::Unsupported),
        comet_harness::HarnessCli::Missing => return Some(RuntimeUnavailable::NotInstalled),
        comet_harness::HarnessCli::Found(_) | comet_harness::HarnessCli::NotNeeded => {}
    }
    if account.is_some_and(|a| !a.is_empty()) {
        return None;
    }
    match accounts.signed_in(harness) {
        Some(false) => Some(RuntimeUnavailable::SignedOut),
        // `None` is a harness with no login concept; `Some(true)` is one with a
        // login. Both can start.
        _ => None,
    }
}

/// The runtime catalog `ListBoardRuntimes` answers with: the board's names, in
/// the board's order, each stamped with whether it could start here.
///
/// Every option is listed whether or not it is available. Filtering would make
/// the picker lie by omission — an operator who expects OpenCode on a box needs
/// to read "not installed", not to find the row missing and wonder whether they
/// misremembered which box it was.
pub fn options(accounts: &AgentAccounts) -> Vec<RuntimeOption> {
    comet_board::runtime::runtime_options()
        .into_iter()
        .map(|option| RuntimeOption {
            unavailable: availability(accounts, option.harness, None),
            ..option
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_accounts::AgentAccountsConfig;

    fn accounts(dir: &std::path::Path) -> AgentAccounts {
        AgentAccounts::new(AgentAccountsConfig {
            data_dir: dir.join("data"),
            claude_config_dir: dir.join("claude"),
            claude_config_file: dir.join("claude.json"),
            codex_home: dir.join("codex"),
            opencode_auth_file: dir.join("opencode-auth.json"),
        })
    }

    /// The mock harness needs neither a CLI nor a login — it is dispatchable on
    /// purpose (`demo`, the integration tests), and a picker that reported it
    /// as "not installed" would break both.
    #[test]
    fn the_mock_harness_is_always_available() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            availability(&accounts(dir.path()), HarnessId::Mock, None),
            None
        );
    }

    /// Cursor is in the runtime table and has never had an adapter. That is a
    /// different fact from a missing CLI, and saying "not installed" would send
    /// an operator off to install something that would not help.
    #[test]
    fn cursor_is_unsupported_rather_than_uninstalled() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            availability(&accounts(dir.path()), HarnessId::Cursor, None),
            Some(RuntimeUnavailable::Unsupported)
        );
    }

    /// A dispatch naming a slot reads that slot, not the box's own login — so
    /// an empty `~/.codex` must not refuse it. (The slot is checked for real
    /// when the executor materializes it.)
    #[test]
    fn a_named_account_answers_for_itself() {
        let dir = tempfile::tempdir().unwrap();
        let accounts = accounts(dir.path());
        // Only meaningful where the CLI is actually present; where it is not,
        // both answers are `NotInstalled` and the test proves nothing.
        if !matches!(
            comet_harness::locate_cli(HarnessId::Codex),
            comet_harness::HarnessCli::Found(_)
        ) {
            return;
        }
        assert_eq!(
            availability(&accounts, HarnessId::Codex, None),
            Some(RuntimeUnavailable::SignedOut),
            "an empty CODEX_HOME with no OPENAI_API_KEY is signed out"
        );
        assert_eq!(
            availability(&accounts, HarnessId::Codex, Some("8f2c1d0a7b6e4539")),
            None
        );
    }

    /// Availability is stamped onto the board's own catalog — same names, same
    /// order, nothing dropped.
    #[test]
    fn every_board_runtime_is_listed_available_or_not() {
        let dir = tempfile::tempdir().unwrap();
        let listed = options(&accounts(dir.path()));
        let catalog = comet_board::runtime::runtime_options();
        assert_eq!(listed.len(), catalog.len());
        for (got, want) in listed.iter().zip(&catalog) {
            assert_eq!(got.name, want.name);
            assert_eq!(got.harness, want.harness);
        }
        // The one entry whose answer is fixed on every device.
        let cursor = listed.iter().find(|o| o.name == "cursor").unwrap();
        assert_eq!(cursor.unavailable, Some(RuntimeUnavailable::Unsupported));
        assert_eq!(cursor.note(), Some("no adapter in this build"));
    }
}
