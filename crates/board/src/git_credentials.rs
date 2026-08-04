//! Pushing to GitHub with an App's installation token (gh#58).
//!
//! An installation token is a password over HTTPS: the remote is
//! `https://x-access-token:<token>@github.com/{owner}/{repo}`. Writing that URL
//! anywhere is the mistake this module exists to avoid.
//!
//! - **Not into `.git/config`.** The token dies in an hour; the checkout does
//!   not. A remote URL with a credential in it outlives the credential, and
//!   every worktree the board cuts would carry a copy.
//! - **Not into argv.** `ps` is world-readable, and #55 means the box can have
//!   several people on it. `git -c http.extraHeader=Authorization:…` and
//!   `git push https://x-access-token:tok@…` both put the secret where any
//!   local process can read it for as long as the push runs.
//! - **Not into the environment either**, for the same reason on Linux, where
//!   `/proc/<pid>/environ` is readable by the same user's other processes.
//!
//! What is left is git's askpass protocol: the URL carries the *username*
//! (`x-access-token`, GitHub's fixed name for this), git runs `$GIT_ASKPASS`
//! when it needs the password, and the helper writes the token to its own
//! stdout — read by git, seen by nobody else. The helper is this same binary
//! (`comet-board git-askpass`), which mints at push time, so the token is
//! never older than the push.
//!
//! Nothing the board itself owns runs `git push` today — the agent in the pane
//! does, with the device's own git credentials. This is the mechanism, tested
//! and ready for the caller that wants it; handing it to a dispatched agent
//! means threading it through the harness env and is deliberately a separate
//! change.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::config::{Credentials, Paths};
use crate::sources::github_app::TokenProvider;

/// Which repo the askpass helper should mint for. It gets the prompt string and
/// nothing else, and the prompt names the host, not the repository.
pub const ASKPASS_REPO_ENV: &str = "COMET_BOARD_ASKPASS_REPO";

/// GitHub's fixed username for installation-token HTTPS auth. The token is the
/// password; this is not a secret and belongs in the URL.
pub const APP_USERNAME: &str = "x-access-token";

/// The remote to push to — username only, no password.
///
/// The username has to be in the URL rather than left to askpass: with no
/// username git prompts for one first, and a helper answering both prompts off
/// one env var is a helper that will one day answer the wrong one.
pub fn push_url(repo: &str) -> String {
    format!("https://{APP_USERNAME}@github.com/{repo}.git")
}

/// The environment a `git push` needs to authenticate as the board's credential.
///
/// `exe` is this binary. Together with [`push_url`] this is the whole contract:
/// no persisted remote, no secret in argv, and nothing left behind in the
/// checkout when the token expires an hour later.
pub fn push_env(exe: &Path, repo: &str) -> Vec<(String, String)> {
    vec![
        ("GIT_ASKPASS".into(), askpass_command(exe)),
        (ASKPASS_REPO_ENV.into(), repo.to_string()),
        // Fail rather than block. Without this, a helper that cannot mint
        // leaves git waiting on a terminal that a dispatched push does not have.
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        // Disable whatever credential helper the box is configured with — on
        // macOS that is the keychain, which would happily store an hourly token
        // forever and hand it back after it stops working. Passed as config
        // through the environment rather than as `-c` on the command line,
        // which keeps it off argv with everything else.
        ("GIT_CONFIG_COUNT".into(), "1".into()),
        ("GIT_CONFIG_KEY_0".into(), "credential.helper".into()),
        ("GIT_CONFIG_VALUE_0".into(), String::new()),
    ]
}

/// `GIT_ASKPASS` names one command, and git runs anything with a space in it
/// through `sh -c '<cmd> "$@"'`. That is how the subcommand gets there — and
/// why the path has to be quoted: an executable under `/Applications/Some
/// App.app/…` would otherwise arrive as two words.
fn askpass_command(exe: &Path) -> String {
    format!("{} git-askpass", sh_quote(&exe.display().to_string()))
}

/// Single-quote for `sh`, the way `'` itself has to be escaped there: end the
/// quote, emit an escaped quote, start a new one.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// What `comet-board git-askpass` answers.
///
/// Git asks for a username when the URL carries none and for a password when it
/// does; [`push_url`] always carries one, but answering both keeps the helper
/// correct if somebody points it at a bare URL.
pub fn askpass(paths: &Paths, prompt: &str, repo: Option<&str>) -> Result<String> {
    if prompt.to_ascii_lowercase().contains("username") {
        return Ok(APP_USERNAME.to_string());
    }
    let repo = repo
        .filter(|r| !r.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{ASKPASS_REPO_ENV} is not set — nothing to mint for"))?;
    let credentials = Credentials::load(paths);
    token_for_push(&crate::sources::github::provider(&credentials)?, repo)
}

/// A password for pushing to `repo`.
///
/// Under an App this is a freshly-minted installation token. Under a personal
/// access token it is that token, which is exactly what somebody self-hosting
/// on a PAT already pushes with.
pub fn token_for_push(auth: &TokenProvider, repo: &str) -> Result<String> {
    match auth {
        TokenProvider::App(app) => app.token_for_repo(repo),
        TokenProvider::Static(t) => Ok(t.clone()),
        TokenProvider::Anonymous => Err(anyhow::anyhow!(
            "no GitHub credential to push {repo} with — set GITHUB_TOKEN, or a \
             GITHUB_APP_ID and GITHUB_APP_PRIVATE_KEY_PATH pair"
        )),
    }
}

/// This binary, for [`push_env`]. Falls back to the name on `PATH` when the
/// current exe cannot be resolved.
pub fn self_exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("comet-board"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::github_app::test_app;

    #[test]
    fn the_push_url_carries_the_username_and_never_the_token() {
        let url = push_url("bredebjorhovd/comet");
        assert_eq!(
            url,
            "https://x-access-token@github.com/bredebjorhovd/comet.git"
        );
        // A colon inside the userinfo is what a password looks like in a URL.
        let userinfo = url
            .trim_start_matches("https://")
            .split('@')
            .next()
            .unwrap();
        assert!(!userinfo.contains(':'), "the URL carries a password: {url}");
    }

    #[test]
    fn the_push_environment_carries_no_secret_at_all() {
        // The point of the whole module: everything here is safe to appear in
        // `ps`, in a log line, or in `/proc/<pid>/environ`. The token only ever
        // exists on the helper's stdout.
        let env = push_env(Path::new("/usr/local/bin/comet-board"), "o/r");
        let joined = env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(joined.contains("GIT_ASKPASS='/usr/local/bin/comet-board' git-askpass"));
        assert!(joined.contains("COMET_BOARD_ASKPASS_REPO=o/r"));
        assert!(joined.contains("GIT_TERMINAL_PROMPT=0"));
        assert!(
            !joined.contains("ghs_"),
            "no token in the environment: {joined}"
        );
        assert!(
            !joined.contains("ghp_"),
            "no token in the environment: {joined}"
        );
    }

    #[test]
    fn an_executable_path_with_a_space_in_it_still_reaches_the_subcommand() {
        // git runs a GIT_ASKPASS containing a space through `sh -c`, so an
        // unquoted `/Applications/My App/comet-board` would arrive as two words
        // and the helper would never run — leaving git to fail the push with a
        // credential error that says nothing about why.
        assert_eq!(
            askpass_command(Path::new("/Applications/My App/comet-board")),
            "'/Applications/My App/comet-board' git-askpass"
        );
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn the_box_credential_helper_is_switched_off_for_the_push() {
        // macOS stores into the keychain by default. An installation token cached
        // there outlives its hour and is then handed back to every later push.
        let env = push_env(Path::new("comet-board"), "o/r");
        let by_key: std::collections::BTreeMap<_, _> = env.into_iter().collect();
        assert_eq!(
            by_key.get("GIT_CONFIG_COUNT").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            by_key.get("GIT_CONFIG_KEY_0").map(String::as_str),
            Some("credential.helper")
        );
        assert_eq!(
            by_key.get("GIT_CONFIG_VALUE_0").map(String::as_str),
            Some("")
        );
    }

    #[test]
    fn a_username_prompt_is_answered_without_minting_anything() {
        let paths = Paths {
            config_dir: std::path::PathBuf::from("/nonexistent"),
            state_dir: std::path::PathBuf::from("/nonexistent"),
        };
        // No credential is configured and none is needed: the username is fixed.
        assert_eq!(
            askpass(&paths, "Username for 'https://github.com': ", None).unwrap(),
            "x-access-token"
        );
    }

    #[test]
    fn a_push_mints_an_installation_token_for_that_repo() {
        let (app, api, _) = test_app(&[("o/r", 42)]);
        let auth = TokenProvider::App(app);
        assert_eq!(token_for_push(&auth, "o/r").unwrap(), "ghs_token_1");
        // The push and the REST client share one provider, so a push right after
        // a poll reuses the poll's token rather than minting a second.
        assert_eq!(token_for_push(&auth, "o/r").unwrap(), "ghs_token_1");
        assert_eq!(api.mints(), 1);
    }

    #[test]
    fn a_personal_access_token_still_pushes() {
        let auth = TokenProvider::Static("ghp_static".into());
        assert_eq!(token_for_push(&auth, "o/r").unwrap(), "ghp_static");
    }

    #[test]
    fn with_no_credential_a_push_fails_rather_than_pushing_anonymously() {
        let err = token_for_push(&TokenProvider::Anonymous, "o/r")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no GitHub credential"), "{err}");
    }
}
