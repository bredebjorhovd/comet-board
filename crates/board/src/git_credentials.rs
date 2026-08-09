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
//! The caller is a **dispatched agent** (gh#68): [`agent_env`] is stamped onto
//! the harness child by the engine, so the agent's `git push` authenticates as
//! the board's App instead of as whoever owns the box. That changes the blast
//! radius of the askpass helper — it now answers for every `git` the agent
//! runs, not for one push the board issued — so [`askpass`] refuses any prompt
//! naming a host other than github.com. An installation token handed to a
//! `git fetch https://gitlab.com/…` would be a credential sent to a stranger.
//!
//! `gh` is the other half of "open a pull request", and it has no askpass: it
//! reads `GH_TOKEN` out of its environment, once, at startup. A token minted
//! when the agent spawned is an hour older than the `gh pr create` at the end
//! of a long run, so instead of exporting one, [`install_gh_shim`] writes a
//! `gh` wrapper onto the front of the child's PATH that mints at the moment gh
//! runs. The token does reach that gh process's environment — unavoidable,
//! that is gh's only interface — but it lives for one command rather than for
//! the run, and it is never in the agent's own environment.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::{Credentials, Paths};
use crate::sources::github_app::TokenProvider;

/// Which repo the askpass helper should mint for. It gets the prompt string and
/// nothing else, and the prompt names the host, not the repository.
///
/// The `gh` shim reads the same variable: one run authenticates for one repo,
/// whichever tool is asking.
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

/// [`push_env`], plus the two variables that tell the helper *which board* it
/// is helping — the whole environment a dispatched agent needs (gh#68).
///
/// The helper is a fresh `comet-board` process with none of the engine's
/// arguments, so without these it would resolve its own directories from
/// `COMET_DATA_DIR` and find a different board's `.env` (or none) on any device
/// started with a data dir of its own. Both are paths, not secrets, and the
/// resolved pair is passed rather than the data dir so the helper cannot
/// re-derive them differently than the engine did.
///
/// This pair is also why no other resolution may read them (gh#190): every
/// process a dispatched agent starts inherits it, including `cargo test`.
/// [`Paths::discover`] is the one reader, and a helper is what it is for.
pub fn agent_env(exe: &Path, repo: &str, paths: &Paths) -> Vec<(String, String)> {
    let mut env = push_env(exe, repo);
    env.push((
        crate::config::CONFIG_DIR_ENV.into(),
        paths.config_dir.display().to_string(),
    ));
    env.push((
        crate::config::STATE_DIR_ENV.into(),
        paths.state_dir.display().to_string(),
    ));
    env
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
///
/// Since gh#68 this runs for every `git` a dispatched agent runs — a clone of
/// somebody's GitLab mirror included — so the host in the prompt is checked
/// first. The credential is a GitHub App installation token and there is no
/// such thing as sending it to the wrong host by accident.
pub fn askpass(paths: &Paths, prompt: &str, repo: Option<&str>) -> Result<String> {
    if let Some(host) = prompt_host(prompt)
        && !is_github_host(host)
    {
        anyhow::bail!(
            "refusing to answer a credential prompt for {host}: this is a GitHub \
             App installation token and belongs to github.com only"
        );
    }
    if prompt.to_ascii_lowercase().contains("username") {
        return Ok(APP_USERNAME.to_string());
    }
    let repo = repo
        .filter(|r| !r.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{ASKPASS_REPO_ENV} is not set — nothing to mint for"))?;
    token(paths, repo)
}

/// A live credential for `repo`, read off this box's board configuration —
/// what both helper subcommands (`git-askpass`, `gh-token`) print.
pub fn token(paths: &Paths, repo: &str) -> Result<String> {
    let credentials = Credentials::load(paths);
    token_for_push(&crate::sources::github::provider(&credentials)?, repo)
}

/// The host inside a git credential prompt — `Password for
/// 'https://x-access-token@github.com': ` → `github.com`. `None` when the
/// prompt names no URL at all, which is a form of the prompt we do not know
/// and must not read a hostname into.
fn prompt_host(prompt: &str) -> Option<&str> {
    let after_scheme = prompt.split("://").nth(1)?;
    let authority = after_scheme
        .split(['/', '\'', '"', ' '])
        .next()
        .filter(|a| !a.is_empty())?;
    // `user@host`, and a port is not part of the name.
    let host = authority.rsplit('@').next()?;
    Some(host.split(':').next().unwrap_or(host))
}

/// github.com itself, or one of its subdomains (`gist.github.com`). A GitHub
/// Enterprise host is deliberately *not* accepted: the App this mints for is
/// registered on github.com and its API constant says so.
fn is_github_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "github.com" || host.ends_with(".github.com")
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

// ---------------------------------------------------------------------------
// gh
// ---------------------------------------------------------------------------

/// The `gh` wrapper's text: mint, export, exec the real thing.
///
/// Everything about it is a fallback that leaves the box's own setup alone. An
/// operator-set `GH_TOKEN`/`GITHUB_TOKEN` wins (they said what to authenticate
/// as); a mint that fails leaves the variable unset rather than empty, so a
/// developer machine with `gh auth login` behaves exactly as it did; and a
/// `GH_HOST` pointing at an Enterprise instance is left alone for the same
/// reason [`askpass`] refuses a non-github.com prompt.
pub fn gh_shim_script(board_exe: &Path, gh: &Path) -> String {
    SHIM.replace("@REPO_ENV@", ASKPASS_REPO_ENV)
        .replace("@BOARD@", &sh_quote(&board_exe.display().to_string()))
        .replace("@GH@", &sh_quote(&gh.display().to_string()))
}

const SHIM: &str = r#"#!/bin/sh
# Generated by comet-board (gh#68). Rewritten on dispatch — edits are lost.
#
# `gh` reads GH_TOKEN once, at startup, and an installation token lives an hour;
# an agent's run does not. So the mint happens here, at the moment gh runs, the
# way GIT_ASKPASS mints at the moment git pushes.
if [ -z "$GH_TOKEN" ] && [ -z "$GITHUB_TOKEN" ] && [ -n "$@REPO_ENV@" ] &&
   { [ -z "$GH_HOST" ] || [ "$GH_HOST" = "github.com" ]; }
then
    _comet_token=$(@BOARD@ gh-token 2>/dev/null) || _comet_token=
    if [ -n "$_comet_token" ]; then
        GH_TOKEN=$_comet_token
        export GH_TOKEN
    fi
    unset _comet_token
fi
exec @GH@ "$@"
"#;

/// Write the `gh` shim into `dir` and answer the directory to put on the front
/// of the agent's PATH.
///
/// Written through a temporary file and renamed into place: the shim can be
/// executing (some other run's `gh pr list`) while this rewrites it, and a
/// rename swaps the name rather than the open file.
#[cfg(unix)]
pub fn install_gh_shim(dir: &Path, board_exe: &Path, gh: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let script = gh_shim_script(board_exe, gh);
    let path = dir.join("gh");
    // Unchanged is the common case (one shim per device, rewritten per
    // dispatch): skip the write so a running `gh` is not even briefly racing a
    // rename.
    if std::fs::read_to_string(&path).is_ok_and(|existing| existing == script) {
        return Ok(dir.to_path_buf());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    // Per-process temp name: two engines (or two runs starting together) must
    // not rename each other's half-written file into place.
    let tmp = dir.join(format!("gh.{}.tmp", std::process::id()));
    std::fs::write(&tmp, &script).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("making {} executable", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("installing {}", path.display()))?;
    Ok(dir.to_path_buf())
}

#[cfg(not(unix))]
pub fn install_gh_shim(_dir: &Path, _board_exe: &Path, _gh: &Path) -> Result<PathBuf> {
    anyhow::bail!("the gh shim is a shell script; only unix boxes dispatch agents")
}

/// Find `comet-board`: an explicit override, then beside the running binary
/// (how it is installed next to the engine), then PATH (how it is run by hand).
pub fn resolve_board_exe() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(BOARD_EXE_ENV).filter(|p| !p.is_empty()) {
        let p = PathBuf::from(p);
        return p.exists().then_some(p);
    }
    let name = if cfg!(windows) {
        "comet-board.exe"
    } else {
        "comet-board"
    };
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(name)))
        .filter(|p| p.exists())
        .or_else(|| in_path(name, None))
}

/// Naming the helper binary outright, for a layout neither lookup above finds.
pub const BOARD_EXE_ENV: &str = "COMET_BOARD_EXECUTABLE";

/// The directory a dispatched agent needs on its PATH to type `comet-board`
/// (gh#184).
///
/// Resolved through [`resolve_board_exe`] and then taken as its parent, which
/// buys two things a fixed path would not. It cannot name a directory with no
/// `comet-board` in it — the lookup found the binary there. And it is the same
/// binary `GIT_ASKPASS` is pointed at, so the CLI an agent runs and the helper
/// its pushes run are one copy: on a managed box, `app/<version>/`, the payload
/// the engine itself shipped in.
///
/// The failure this exists for is a PATH nobody looks at. `install.sh` links the
/// CLI into `~/.local/bin`, which the systemd **user** service does not inherit
/// and a non-interactive ssh shell never sources — so a dispatched agent on the
/// box got `command not found` for every board verb the skill told it to run,
/// and simply went on without the board.
pub fn agent_bin_dir() -> Option<PathBuf> {
    bin_dir_of(resolve_board_exe())
}

/// The testable half: the lookup is injected, because "no binary anywhere" is a
/// state the machine running the tests is not reliably in either way.
fn bin_dir_of(exe: Option<PathBuf>) -> Option<PathBuf> {
    exe.and_then(|exe| exe.parent().map(Path::to_path_buf))
        .filter(|dir| !dir.as_os_str().is_empty())
}

/// Find the real `gh`, skipping `skip` — the shim directory, which holds a
/// `gh` of our own and would otherwise wrap itself forever.
pub fn resolve_gh(skip: Option<&Path>) -> Option<PathBuf> {
    let name = if cfg!(windows) { "gh.exe" } else { "gh" };
    in_path(name, skip).or_else(|| {
        // Homebrew on either architecture, for a GUI launch whose PATH is the
        // login default rather than a shell's.
        [
            PathBuf::from("/opt/homebrew/bin/gh"),
            PathBuf::from("/usr/local/bin/gh"),
        ]
        .into_iter()
        .find(|p| p.exists())
    })
}

fn in_path(name: &str, skip: Option<&Path>) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    scan(std::env::split_paths(&path), name, skip)
}

/// The PATH walk itself, over an explicit list of directories.
fn scan(dirs: impl Iterator<Item = PathBuf>, name: &str, skip: Option<&Path>) -> Option<PathBuf> {
    dirs.filter(|dir| !dir.as_os_str().is_empty() && Some(dir.as_path()) != skip)
        .map(|dir| dir.join(name))
        .find(|p| p.exists())
}

/// `owner/repo` for a checkout, from its `origin` remote.
///
/// The fallback for a task whose id does not name a repo (a Linear ticket
/// dispatched into a git space). It is also the most honest answer available:
/// the remote is where the branch will actually be pushed, so it is the repo
/// the token has to be scoped to.
pub fn repo_for_checkout(path: &str) -> Option<String> {
    crate::adopt::git_remote(path)
        .as_deref()
        .and_then(crate::adopt::github_slug)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::github_app::test_app;

    /// gh#184: the directory an agent gets on PATH is the one the resolved
    /// binary sits in — so it always holds a `comet-board`, and it is the same
    /// copy the askpass helper runs as.
    #[test]
    fn the_agents_bin_dir_is_the_resolved_binarys_own() {
        assert_eq!(
            bin_dir_of(Some(PathBuf::from(
                "/home/comet/.comet-native/app/0.3.5/comet-board"
            ))),
            Some(PathBuf::from("/home/comet/.comet-native/app/0.3.5"))
        );
        // No binary is no directory — never a bare "." that would put the
        // agent's own cwd on the front of its PATH.
        assert_eq!(bin_dir_of(None), None);
        assert_eq!(bin_dir_of(Some(PathBuf::from("comet-board"))), None);
    }

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

    /// gh#68 put this helper in a dispatched agent's environment, where it
    /// answers for every `git` the agent runs — including a clone of somebody
    /// else's mirror. An installation token sent to a host that is not GitHub
    /// is a credential handed to a stranger.
    #[test]
    fn a_prompt_for_another_host_is_refused_before_anything_is_minted() {
        let paths = Paths {
            config_dir: std::path::PathBuf::from("/nonexistent"),
            state_dir: std::path::PathBuf::from("/nonexistent"),
        };
        for prompt in [
            "Password for 'https://x-access-token@gitlab.com': ",
            "Username for 'https://git.example.com': ",
            // The classic near-miss: a host that merely ends in the right
            // letters. `github.com.evil.test` is not github.com.
            "Password for 'https://github.com.evil.test': ",
        ] {
            let err = askpass(&paths, prompt, Some("o/r"))
                .expect_err("answered a prompt for the wrong host")
                .to_string();
            assert!(err.contains("refusing"), "{prompt} → {err}");
        }
        // github.com and its subdomains still go through.
        assert_eq!(
            prompt_host("Password for 'https://x-access-token@github.com': "),
            Some("github.com")
        );
        assert!(is_github_host("gist.github.com"));
        assert!(!is_github_host("notgithub.com"));
        // A port is not part of the name, and a prompt with no URL in it names
        // no host to check.
        assert_eq!(
            prompt_host("Password for 'https://github.com:443/o/r'"),
            Some("github.com")
        );
        assert_eq!(prompt_host("Password: "), None);
    }

    /// The helper is a fresh process with none of the engine's arguments. On a
    /// device started with a data dir of its own, that means it would read a
    /// different board's `.env` — or none — and mint nothing.
    #[test]
    fn the_agent_environment_names_the_board_the_helper_should_read() {
        let paths = Paths {
            config_dir: std::path::PathBuf::from("/data/board"),
            state_dir: std::path::PathBuf::from("/data/board/state"),
        };
        let env: std::collections::BTreeMap<_, _> =
            agent_env(Path::new("/usr/local/bin/comet-board"), "o/r", &paths)
                .into_iter()
                .collect();
        assert_eq!(
            env.get("COMET_BOARD_CONFIG_DIR").map(String::as_str),
            Some("/data/board")
        );
        assert_eq!(
            env.get("COMET_BOARD_STATE_DIR").map(String::as_str),
            Some("/data/board/state")
        );
        // Still everything `push_env` promised.
        assert_eq!(
            env.get("COMET_BOARD_ASKPASS_REPO").map(String::as_str),
            Some("o/r")
        );
        assert_eq!(
            env.get("GIT_TERMINAL_PROMPT").map(String::as_str),
            Some("0")
        );
    }

    /// The `gh` half. What matters is that the token is fetched *inside* the
    /// wrapper rather than exported into the agent's environment, and that
    /// every path in it is quoted for `sh`.
    #[test]
    fn the_gh_shim_mints_per_invocation_and_defers_to_a_configured_token() {
        let script = gh_shim_script(
            Path::new("/Applications/My App/comet-board"),
            Path::new("/opt/homebrew/bin/gh"),
        );
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(
            script.contains("_comet_token=$('/Applications/My App/comet-board' gh-token"),
            "{script}"
        );
        assert!(
            script.contains("exec '/opt/homebrew/bin/gh' \"$@\""),
            "{script}"
        );
        // A token the operator set wins, and so does an Enterprise host.
        assert!(script.contains("[ -z \"$GH_TOKEN\" ]"), "{script}");
        assert!(script.contains("[ -z \"$GITHUB_TOKEN\" ]"), "{script}");
        assert!(
            script.contains("[ \"$GH_HOST\" = \"github.com\" ]"),
            "{script}"
        );
        // The repo comes from the run's own environment — one variable for
        // git and gh both.
        assert!(
            script.contains("[ -n \"$COMET_BOARD_ASKPASS_REPO\" ]"),
            "{script}"
        );
    }

    /// The shim directory goes on the agent's PATH, so a `gh` lookup that did
    /// not skip it would find our own wrapper and write a wrapper that execs
    /// itself — a fork bomb with a helpful comment at the top.
    #[test]
    fn the_gh_lookup_never_resolves_to_the_shim_itself() {
        let dir = std::env::temp_dir().join(format!("comet-ghscan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let shim = dir.join("bin");
        let real = dir.join("usr-bin");
        std::fs::create_dir_all(&shim).unwrap();
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(shim.join("gh"), "#!/bin/sh\n").unwrap();
        std::fs::write(real.join("gh"), "#!/bin/sh\n").unwrap();
        assert_eq!(
            scan([shim.clone(), real.clone()].into_iter(), "gh", Some(&shim)),
            Some(real.join("gh"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The shim is a real script `sh` will run, not just text that looks like
    /// one — including with a space in the path it execs.
    #[test]
    #[cfg(unix)]
    fn the_gh_shim_runs_and_falls_back_when_nothing_can_be_minted() {
        let dir = std::env::temp_dir().join(format!("comet-shim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A "gh" that reports what it was handed, and a minting helper that
        // fails the way an unconfigured board does.
        let fake_gh = dir.join("real gh");
        std::fs::write(
            &fake_gh,
            "#!/bin/sh\necho \"gh:$1 token:${GH_TOKEN:-none}\"\n",
        )
        .unwrap();
        let broken = dir.join("comet-board");
        std::fs::write(&broken, "#!/bin/sh\nexit 1\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            for p in [&fake_gh, &broken] {
                std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let bin = install_gh_shim(&dir.join("bin"), &broken, &fake_gh).unwrap();
        let out = std::process::Command::new(bin.join("gh"))
            .arg("pr")
            .env(ASKPASS_REPO_ENV, "o/r")
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .output()
            .unwrap();
        // A mint that fails leaves GH_TOKEN unset rather than empty, so a box
        // with its own `gh auth login` keeps working.
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "gh:pr token:none"
        );
        let _ = std::fs::remove_dir_all(&dir);
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
