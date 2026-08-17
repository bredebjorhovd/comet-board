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
//! stdout — read by git, seen by nobody else. The helper mints at push time, so
//! the token is never older than the push.
//!
//! **`GIT_ASKPASS` names one executable and git execs it directly** — no shell,
//! no arguments. `#58` was built on the opposite belief (that git ran anything
//! with a space in it through `sh -c '<cmd> "$@"'`, which is what
//! `credential.helper` does) and so put `'<path>' git-askpass` in the variable.
//! git took the whole string as a filename, found no such file, and every
//! dispatched push that reached this code failed with `cannot exec`. gh#233 is
//! the ticket; `tests/askpass_git.rs` is the proof, and it runs a real `git`
//! against a real 401 rather than asserting the shape of a string.
//!
//! So the subcommand cannot ride in the variable, and it rides in a one-line
//! shell script instead — [`install_askpass_shim`], beside the `gh` shim below
//! and for the same kind of reason. A path is all `GIT_ASKPASS` can carry, so a
//! path is all it is given: spaces in it are fine (git execs it, it does not
//! parse it), and there is nothing left for a quoting rule to get wrong.
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
use crate::sources::github::HttpRest;
use crate::sources::github_app::TokenProvider;

/// Which repo the askpass helper should mint for. It gets the prompt string and
/// nothing else, and the prompt names the host, not the repository.
///
/// The `gh` shim reads the same variable: one run authenticates for one repo,
/// whichever tool is asking.
pub const ASKPASS_REPO_ENV: &str = "COMET_BOARD_ASKPASS_REPO";

/// The nonsecret write contract a dispatched chat's credential must still
/// satisfy when a late-mint helper runs. A single variable makes a partial
/// contract impossible; its values are parsed strictly and contain no bearer.
pub const PUSH_CONTRACT_ENV: &str = "COMET_BOARD_PUSH_CONTRACT";

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

/// The canonical remote for a repo — the URL a human would have typed, and the
/// one a checkout should be left holding.
pub fn https_url(repo: &str) -> String {
    format!("https://github.com/{repo}.git")
}

/// How to speak to a repo somebody named: the URL git is given, the URL the
/// checkout keeps, and the repo to mint a credential for (gh#316).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneUrls {
    /// What `git clone` is handed. Carries `x-access-token@` for a GitHub repo,
    /// so askpass has a username to answer against.
    pub auth: String,
    /// What `origin` ends up as — never the authenticating form.
    pub canonical: String,
    /// `owner/repo`, when this is a GitHub repo the board can mint for. `None`
    /// means "not ours to authenticate": the URL is passed through untouched.
    pub slug: Option<String>,
}

/// Read whatever the caller named — `owner/repo`, an HTTPS URL, an SSH remote —
/// into the pair of URLs a clone should use.
///
/// **A GitHub repo is always cloned over HTTPS**, whichever form was asked for.
/// An installation token is a password over HTTPS and nothing else; the SSH form
/// authenticates with the *device's* keys, which on a freshly provisioned box are
/// no keys at all — and its first failure is not even an authentication one, it
/// is `Host key verification failed` against an empty `known_hosts`. That is the
/// gh#316 failure, and rewriting the URL is what removes the whole class of it:
/// the board can only lend the credential it has.
///
/// Anything that is not a GitHub repo — a `file://` origin, a GitLab remote, an
/// absolute path — passes through unchanged and unauthenticated. The board has
/// no credential for it and must not invent a URL for a host it does not know.
///
/// A bare `owner/repo` is read as GitHub's, the way [`crate::onboard::parse_slug`]
/// reads it everywhere else. It is ambiguous with a *relative* path, and that
/// ambiguity resolves the same way `onboard::checkout_path` resolves it: a clone
/// is device-addressed, and a path relative to the engine's working directory on
/// a machine nobody is sitting at is not something anybody means.
pub fn clone_urls(named: &str) -> CloneUrls {
    let named = named.trim();
    match crate::onboard::parse_slug(named).ok() {
        Some(slug) => CloneUrls {
            auth: push_url(&slug),
            canonical: https_url(&slug),
            slug: Some(slug),
        },
        None => CloneUrls {
            auth: named.to_string(),
            canonical: named.to_string(),
            slug: None,
        },
    }
}

/// The environment a `git push` needs to authenticate as the board's credential.
///
/// `askpass` is the shim [`install_askpass_shim`] wrote — a path, because a
/// path is the only thing `GIT_ASKPASS` can hold. Together with [`push_url`]
/// this is the whole contract: no persisted remote, no secret in argv, and
/// nothing left behind in the checkout when the token expires an hour later.
pub fn push_env(askpass: &Path, repo: &str) -> Vec<(String, String)> {
    vec![
        ("GIT_ASKPASS".into(), askpass.display().to_string()),
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
pub fn agent_env(askpass: &Path, repo: &str, paths: &Paths) -> Vec<(String, String)> {
    let mut env = push_env(askpass, repo);
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

/// [`agent_env`] plus the durable capability promise for a board-dispatched
/// chat. Both `git-askpass` and the `gh` wrapper inherit this marker and refuse
/// to emit a credential that no longer satisfies it.
pub fn agent_env_with_contract(
    askpass: &Path,
    repo: &str,
    paths: &Paths,
    contract: comet_proto::GithubPushContract,
) -> Vec<(String, String)> {
    let mut env = agent_env(askpass, repo, paths);
    let value = match (contract.contents_write, contract.workflows_write) {
        (false, false) => "none",
        (true, false) => "contents",
        (true, true) => "contents+workflows",
        (false, true) => "workflows",
    };
    env.push((PUSH_CONTRACT_ENV.into(), value.into()));
    env
}

/// Read the nonsecret contract marker inherited by a credential helper.
/// Missing means this is a non-dispatched operation such as an engine-owned
/// clone; dispatched helpers require `Some` at their CLI boundary.
pub fn push_contract_from_env() -> Result<Option<comet_proto::GithubPushContract>> {
    let Some(value) = std::env::var(PUSH_CONTRACT_ENV).ok() else {
        return Ok(None);
    };
    let contract = match value.as_str() {
        "none" => comet_proto::GithubPushContract {
            contents_write: false,
            workflows_write: false,
        },
        "contents" => comet_proto::GithubPushContract {
            contents_write: true,
            workflows_write: false,
        },
        "contents+workflows" => comet_proto::GithubPushContract {
            contents_write: true,
            workflows_write: true,
        },
        "workflows" => anyhow::bail!(
            "{PUSH_CONTRACT_ENV}=workflows is invalid: workflow writes require content writes"
        ),
        _ => anyhow::bail!(
            "{PUSH_CONTRACT_ENV} has an unknown value; expected none, contents, or contents+workflows"
        ),
    };
    Ok(Some(contract))
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
    askpass_with_contract(paths, prompt, repo, None)
}

/// [`askpass`] with the dispatched chat's durable write contract enforced
/// before its password is returned.
pub fn askpass_with_contract(
    paths: &Paths,
    prompt: &str,
    repo: Option<&str>,
    contract: Option<comet_proto::GithubPushContract>,
) -> Result<String> {
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
    match contract {
        Some(contract) => token_with_contract(paths, repo, contract),
        None => token(paths, repo),
    }
}

/// A live credential for `repo`, read off this box's board configuration —
/// what both helper subcommands (`git-askpass`, `gh-token`) print.
pub fn token(paths: &Paths, repo: &str) -> Result<String> {
    let credentials = Credentials::load(paths);
    token_for_push(&crate::sources::github::provider(&credentials)?, repo)
}

/// A live credential that is returned only when fresh per-repository evidence
/// still satisfies the chat's persisted contract.
pub fn token_with_contract(
    paths: &Paths,
    repo: &str,
    contract: comet_proto::GithubPushContract,
) -> Result<String> {
    let rest = HttpRest::from_paths(paths)?;
    token_with_contract_from(&rest, repo, contract)
}

/// The testable half of [`token_with_contract`]. For an App, the capability
/// probe and returned token share the same provider/cache, so the permission
/// object belongs to the exact token handed to git rather than an unrelated
/// mint.
pub(crate) fn token_with_contract_from(
    rest: &HttpRest,
    repo: &str,
    contract: comet_proto::GithubPushContract,
) -> Result<String> {
    rest.push_capabilities(repo)?.require(contract)?;
    token_for_push(rest.auth(), repo)
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
// The askpass shim
// ---------------------------------------------------------------------------

/// The file name of the askpass shim inside the board's shim directory.
///
/// Deliberately not `git-askpass`: that directory goes on a dispatched agent's
/// PATH, and git treats any `git-<word>` on PATH as a subcommand of its own.
pub const ASKPASS_SHIM: &str = "comet-askpass";

/// The askpass shim's text: hand git's prompt to `comet-board git-askpass`.
///
/// One line of shell, and it exists for one reason — `GIT_ASKPASS` can name an
/// executable and nothing else (see the module header), so the subcommand needs
/// somewhere to live. `exec` rather than a call, so the helper *is* the process
/// git is waiting on and its stdout is git's pipe, unmediated.
pub fn askpass_shim_script(board_exe: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         # Generated by comet-board (gh#233). Rewritten on dispatch — edits are lost.\n\
         #\n\
         # git execs GIT_ASKPASS directly: one executable, no shell, no arguments.\n\
         # The subcommand cannot ride in the variable, so it rides here.\n\
         exec {} git-askpass \"$@\"\n",
        sh_quote(&board_exe.display().to_string())
    )
}

/// Write the askpass shim into `dir` and answer **the shim's own path** —
/// what `GIT_ASKPASS` is set to.
///
/// The `gh` shim answers with its directory because that one goes on PATH; this
/// one is never looked up by name, only exec'd by git.
#[cfg(unix)]
pub fn install_askpass_shim(dir: &Path, board_exe: &Path) -> Result<PathBuf> {
    install_shim(dir, ASKPASS_SHIM, &askpass_shim_script(board_exe))
}

#[cfg(not(unix))]
pub fn install_askpass_shim(_dir: &Path, _board_exe: &Path) -> Result<PathBuf> {
    anyhow::bail!("the askpass shim is a shell script; only unix boxes dispatch agents")
}

/// Ask the shim the one question that needs no credential, no network and no
/// board: what username does a GitHub App push with?
///
/// The answer is the fixed [`APP_USERNAME`], so what this actually tests is the
/// part gh#233 was about — that the path in `GIT_ASKPASS` can be exec'd, that
/// what it reaches is a `comet-board` that understands `git-askpass`, and that
/// the answer arrives on stdout. Every layer of the credential path except the
/// mint itself, for the price of one `fork`.
///
/// Called the way git calls it: the prompt as the single argument.
///
/// This is also the barrier that makes the `git push` after it safe (gh#301).
/// [`install_shim`] closes its own handle before this can run, but on Linux one
/// writer is still out of its reach: a *sibling thread* that forks while that
/// handle is open hands the child a copy of it, and the copy lives until that
/// child's own `exec` — `O_CLOEXEC` closes it then, not sooner. An engine
/// dispatching agents forks constantly, so between installing a shim and
/// exec'ing it there can be a descheduled child holding the shim's inode open
/// for writing, and the kernel answers the exec with `ETXTBSY`. No write-side
/// ordering can prevent that: any file this process creates is a file some
/// concurrent fork can inherit a handle to.
///
/// What is true is that the state is *transient by construction* — it ends at
/// the child's exec, with nothing else on the box able to reopen the shim for
/// writing — so the answer is to wait it out, bounded, and then let a real
/// failure through. A successful exec proves the inode has no writers, which is
/// why this being the last thing before a push is what keeps `git`'s own exec of
/// the same file (which nothing can retry) off the same race.
pub fn verify_askpass(askpass: &Path) -> Result<()> {
    let out = run_askpass(askpass, "Username for 'https://github.com': ")
        .with_context(|| format!("running the askpass helper at {}", askpass.display()))?;
    let answer = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "the askpass helper at {} exited {}: {}",
            askpass.display(),
            out.status,
            err.trim().lines().next_back().unwrap_or("no output")
        );
    }
    if answer != APP_USERNAME {
        anyhow::bail!(
            "the askpass helper at {} answered {answer:?} where git expects {APP_USERNAME:?}",
            askpass.display()
        );
    }
    Ok(())
}

/// Exercise the secret-bearing half of the exact askpass handoff a dispatched
/// run will receive, then discard the answer without logging or persisting it.
/// A fixed username only proves the shim can exec; this proves the configured
/// board can still mint/read a credential for this repository.
pub fn verify_push_credential(
    askpass: &Path,
    repo: &str,
    paths: &Paths,
    contract: comet_proto::GithubPushContract,
) -> Result<()> {
    let mut command = std::process::Command::new(askpass);
    command.arg("Password for 'https://x-access-token@github.com': ");
    for (key, value) in agent_env_with_contract(askpass, repo, paths, contract) {
        command.env(key, value);
    }
    let out = exec_waiting_out_busy(&mut command).with_context(|| {
        format!(
            "running the askpass credential handoff at {}",
            askpass.display()
        )
    })?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!(
            "the askpass credential handoff at {} exited {}: {}",
            askpass.display(),
            out.status,
            err.trim().lines().next_back().unwrap_or("no output")
        );
    }
    if out.stdout.iter().all(u8::is_ascii_whitespace) {
        anyhow::bail!(
            "the askpass credential handoff at {} returned no credential",
            askpass.display()
        );
    }
    Ok(())
}

/// How long [`verify_askpass`] will wait out an `ETXTBSY` before reporting it.
///
/// Generous next to what it waits for (a forked child that has not reached its
/// `exec` yet — microseconds, or a scheduler quantum on a loaded CI box) and
/// small next to the push it precedes. A wait that ends is the point: a
/// permissions bug that no amount of waiting fixes has to still be an error.
const EXEC_BUSY_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// Run the shim the way git runs it, waiting out `ETXTBSY` (see
/// [`verify_askpass`] for why that is a wait and not a retry-around-a-defect).
fn run_askpass(askpass: &Path, prompt: &str) -> std::io::Result<std::process::Output> {
    exec_waiting_out_busy(std::process::Command::new(askpass).arg(prompt))
}

/// Run `cmd` to completion, waiting out an `ETXTBSY` that a sibling thread's
/// `fork` put there, for at most [`EXEC_BUSY_BUDGET`].
///
/// The race is the one [`verify_askpass`] documents, and it belongs to the
/// *process*, not to askpass: any thread that execs a file this process has
/// recently written can be refused because some other thread forked while the
/// write handle was open. So this is written against a `Command` rather than
/// against a shim path — gh#385 was the `gh` shim's own test hitting it, one
/// caller away from the code that already knew about it.
///
/// Nothing else is waited on. No such file, not executable, exec'd and exited
/// non-zero for a reason of its own — each is answered immediately, because none
/// of them is transient and reporting them is what a run is for.
///
/// `cmd` is re-run rather than rebuilt: `output` takes `&mut self`, and every
/// attempt must be the same command, or a wait would be hiding a difference.
fn exec_waiting_out_busy(cmd: &mut std::process::Command) -> std::io::Result<std::process::Output> {
    let mut waited = std::time::Duration::ZERO;
    let mut nap = std::time::Duration::from_millis(1);
    loop {
        let attempt = cmd.output();
        let busy = match &attempt {
            // The thing itself could not be exec'd.
            Err(e) => e.kind() == std::io::ErrorKind::ExecutableFileBusy,
            // It ran, and an `exec` *inside* it could not be: the same refusal
            // one level down, on whatever a shim reaches. A payload install
            // writes that binary and the engine dispatches through it, so it is
            // the same window as the shim's own.
            Ok(out) => !out.status.success() && exec_said_busy(&out.stderr),
        };
        if !busy || waited >= EXEC_BUSY_BUDGET {
            return attempt;
        }
        std::thread::sleep(nap);
        waited += nap;
        nap = (nap * 2).min(std::time::Duration::from_millis(50));
    }
}

/// Did a shell report `ETXTBSY` for the thing it was asked to `exec`?
///
/// A shell has only its exit status (126, mostly) and this line of stderr to say
/// it with, so the message is what there is to read. Under a locale that
/// translates `strerror` this stops recognising it, and the failure is then
/// reported rather than waited out — the same behaviour as before gh#301, which
/// is the right way for a heuristic like this to degrade.
fn exec_said_busy(stderr: &[u8]) -> bool {
    String::from_utf8_lossy(stderr)
        .to_ascii_lowercase()
        .contains("text file busy")
}

// ---------------------------------------------------------------------------
// gh
// ---------------------------------------------------------------------------

/// The `gh` wrapper's text: mint, export, exec the real thing.
///
/// In a board-dispatched environment the repo marker is present, and the
/// wrapper must mint the board credential or fail. It explicitly discards
/// inherited `GH_TOKEN` / `GITHUB_TOKEN`; falling back to the box login would
/// violate the board-credential-only delivery contract.
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
if [ -n "$@REPO_ENV@" ]
then
    if [ -n "$GH_HOST" ] && [ "$GH_HOST" != "github.com" ]; then
        echo "comet-board: refusing board credential handoff for GH_HOST=$GH_HOST" >&2
        exit 1
    fi
    unset GH_TOKEN GITHUB_TOKEN
    _comet_token=$(@BOARD@ gh-token 2>/dev/null) || {
        echo "comet-board: could not mint the board credential" >&2
        exit 1
    }
    if [ -z "$_comet_token" ]; then
        echo "comet-board: board credential mint returned no token" >&2
        exit 1
    fi
    GH_TOKEN=$_comet_token
    export GH_TOKEN
    unset _comet_token
fi
exec @GH@ "$@"
"#;

/// Write the `gh` shim into `dir` and answer the directory to put on the front
/// of the agent's PATH.
#[cfg(unix)]
pub fn install_gh_shim(dir: &Path, board_exe: &Path, gh: &Path) -> Result<PathBuf> {
    install_shim(dir, "gh", &gh_shim_script(board_exe, gh))?;
    Ok(dir.to_path_buf())
}

/// A final, process-local guard around every git invocation made by a
/// dispatched run. Codex may rebuild a model tool's environment after its
/// app-server was spawned; the values embedded here cannot be replaced by a
/// shell snapshot because this shim reapplies them immediately before exec.
#[cfg(unix)]
pub fn install_git_shim(
    dir: &Path,
    git: &Path,
    askpass: &Path,
    repo: &str,
    paths: &Paths,
    contract: comet_proto::GithubPushContract,
    chat: &str,
) -> Result<PathBuf> {
    let mut vars = agent_env_with_contract(askpass, repo, paths, contract);
    vars.push(("COMET_BOARD_CHAT_ID".into(), chat.to_string()));
    let exports = vars
        .into_iter()
        .map(|(key, value)| format!("{key}={}; export {key}", sh_quote(&value)))
        .collect::<Vec<_>>()
        .join("\n");
    let script = format!(
        "#!/bin/sh\n# Generated by comet-board (gh#488). Edits are lost.\n{exports}\nexec {} \"$@\"\n",
        sh_quote(&git.display().to_string())
    );
    install_shim(dir, "git", &script)?;
    Ok(dir.to_path_buf())
}

/// Write one executable shim into `dir`, atomically, and answer its path.
///
/// Written through a temporary file and renamed into place: the shim can be
/// executing (some other run's `gh pr list`, some other run's push) while this
/// rewrites it, and a rename swaps the name rather than the open file.
///
/// Every caller execs what this returns, so the write is ordered for that and
/// not just for durability (gh#301). By the time this function returns:
///
/// - the file is already mode `0o755`, because the mode is asked for at `open`
///   and confirmed on the open handle rather than chmod'd onto a path
///   afterwards — a shim that is briefly `0o644` is a shim some other run can
///   fail to exec;
/// - its contents are on disk, `sync_all`'d before the rename rather than left
///   for the page cache to flush under a name somebody is already exec'ing;
/// - and **the writable handle is closed**, before the rename and so before the
///   path this answers with exists at all. A handle open for writing on the
///   shim's inode is precisely what makes an exec of it fail `ETXTBSY`, so it
///   must not outlive this function. See [`verify_askpass`] for the one writer
///   this ordering cannot reach.
#[cfg(unix)]
fn install_shim(dir: &Path, name: &str, script: &str) -> Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let path = dir.join(name);
    // Unchanged is the common case (one shim per device, rewritten per
    // dispatch): skip the write so a running shim is not even briefly racing a
    // rename.
    if std::fs::read_to_string(&path).is_ok_and(|existing| existing == script) {
        return Ok(path);
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    // A temp name of this call's own: two engines (or two runs starting
    // together) must not rename each other's half-written file into place, and
    // the pid alone is not enough for that — one engine dispatching two agents
    // at once installs from two threads with the same pid, and `create_new`
    // below would then fail one of the dispatches rather than let it through.
    static NTH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nth = NTH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!("{name}.{}.{nth}.tmp", std::process::id()));
    // Only reachable through a recycled pid, after a run that died between the
    // create and the rename — but a leftover would then refuse every install on
    // this box until somebody swept the directory by hand.
    let _ = std::fs::remove_file(&tmp);
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.write_all(script.as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
        // `mode` above is narrowed by the umask; this is on the handle, so
        // there is no window in which the name exists at another mode.
        file.set_permissions(std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("making {} executable", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("flushing {}", tmp.display()))?;
        // Dropped here, deliberately and before the rename below.
    }
    std::fs::rename(&tmp, &path).with_context(|| format!("installing {}", path.display()))?;
    Ok(path)
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

pub fn resolve_git(skip: Option<&Path>) -> Option<PathBuf> {
    let name = if cfg!(windows) { "git.exe" } else { "git" };
    in_path(name, skip).or_else(|| {
        [PathBuf::from("/usr/bin/git"), PathBuf::from("/opt/homebrew/bin/git")]
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

    /// A scratch directory of this test's own, emptied first.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("comet-cred-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

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

    /// gh#316. Whatever form the repo was named in, a clone of it goes over
    /// HTTPS with the board's username in the URL — that is the only form the
    /// credential works over, and the SSH form on a fresh box fails on host keys
    /// before it ever reaches authentication.
    #[test]
    fn a_github_repo_clones_over_https_however_it_was_named() {
        for named in [
            "bredebjorhovd/itsm-agent",
            "https://github.com/bredebjorhovd/itsm-agent",
            "https://github.com/bredebjorhovd/itsm-agent.git",
            "git@github.com:bredebjorhovd/itsm-agent.git",
            "ssh://git@github.com/bredebjorhovd/itsm-agent.git",
            "  https://github.com/bredebjorhovd/itsm-agent/  ",
        ] {
            let urls = clone_urls(named);
            assert_eq!(
                urls.slug.as_deref(),
                Some("bredebjorhovd/itsm-agent"),
                "{named} named no repo to mint for"
            );
            assert_eq!(
                urls.auth, "https://x-access-token@github.com/bredebjorhovd/itsm-agent.git",
                "{named}"
            );
            // What the checkout is left holding: no userinfo, no ssh.
            assert_eq!(
                urls.canonical, "https://github.com/bredebjorhovd/itsm-agent.git",
                "{named}"
            );
        }
    }

    /// The board has one credential and it is GitHub's. Anything else is
    /// somebody else's remote: cloned exactly as it was written, with nothing
    /// of ours attached to it.
    #[test]
    fn a_remote_that_is_not_github_is_passed_through_untouched() {
        for named in [
            "file:///srv/mirrors/thing.git",
            "git@gitlab.com:offhand/tally.git",
            "https://gitlab.com/offhand/tally.git",
            "/home/comet/src/thing",
        ] {
            let urls = clone_urls(named);
            assert_eq!(urls.slug, None, "{named} was read as a GitHub repo");
            assert_eq!(urls.auth, named);
            assert_eq!(urls.canonical, named);
        }
    }

    #[test]
    fn the_push_environment_carries_no_secret_at_all() {
        // The point of the whole module: everything here is safe to appear in
        // `ps`, in a log line, or in `/proc/<pid>/environ`. The token only ever
        // exists on the helper's stdout.
        let env = push_env(Path::new("/state/bin/comet-askpass"), "o/r");
        let joined = env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(joined.contains("GIT_ASKPASS=/state/bin/comet-askpass"));
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

    /// gh#233. `GIT_ASKPASS` holds a path and nothing else — no subcommand, no
    /// quoting, nothing for a shell to split, because no shell is involved.
    /// The rule is mechanical enough to assert: whatever is in there has to
    /// name a file on disk.
    #[test]
    #[cfg(unix)]
    fn the_askpass_variable_is_a_bare_path_to_something_that_exists() {
        let dir = scratch("askpass-env");
        let shim = install_askpass_shim(&dir, Path::new("/Applications/My App/comet-board"))
            .expect("shim installed");
        let env: std::collections::BTreeMap<_, _> = push_env(&shim, "o/r").into_iter().collect();
        let value = env.get("GIT_ASKPASS").expect("GIT_ASKPASS").clone();
        assert_eq!(value, shim.display().to_string());
        assert!(
            Path::new(&value).is_file(),
            "GIT_ASKPASS does not name a file git can exec: {value}"
        );
        // The subcommand rides in the script, with the path quoted for the
        // shell that *does* read it.
        let script = std::fs::read_to_string(&shim).unwrap();
        assert!(
            script.contains("exec '/Applications/My App/comet-board' git-askpass \"$@\""),
            "{script}"
        );
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn git_shim_reapplies_the_dispatch_contract_at_the_exec_boundary() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("git-guard");
        let marker = dir.join("origin-changed");
        let real = dir.join("real-git");
        std::fs::write(
            &real,
            format!(
                "#!/bin/sh\n[ \"$COMET_BOARD_CHAT_ID\" = chat-488 ] || exit 21\n[ \"$COMET_BOARD_ASKPASS_REPO\" = owner/repo ] || exit 22\n[ \"$GIT_CONFIG_KEY_0\" = credential.helper ] || exit 23\n[ -x \"$GIT_ASKPASS\" ] || exit 24\nprintf changed > {}\n",
                sh_quote(&marker.display().to_string())
            ),
        )
        .unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
        let askpass = dir.join("askpass");
        std::fs::write(&askpass, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&askpass, std::fs::Permissions::from_mode(0o755)).unwrap();
        let paths = Paths::under(&dir).unwrap();
        let bin = install_git_shim(
            &dir.join("bin"),
            &real,
            &askpass,
            "owner/repo",
            &paths,
            comet_proto::GithubPushContract { contents_write: true, workflows_write: true },
            "chat-488",
        )
        .unwrap();
        let status = std::process::Command::new(bin.join("git"))
            .env("COMET_BOARD_CHAT_ID", "hostile")
            .env("GIT_ASKPASS", "/ambient")
            .status()
            .unwrap();
        assert!(status.success());
        assert!(marker.exists(), "the guarded origin mutation did not occur");
    }

    /// The shim is a script `sh` will run, end to end: git's prompt in, the
    /// helper's answer out.
    #[test]
    #[cfg(unix)]
    fn the_askpass_shim_hands_gits_prompt_to_the_helper_and_answers_on_stdout() {
        let dir = scratch("askpass-run");
        // A "comet-board" that answers the way the real one does, and reports
        // the argv it was given.
        let board = dir.join("comet board");
        std::fs::write(
            &board,
            "#!/bin/sh\necho \"$1\" >&2\necho \"$2\" >&2\necho x-access-token\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&board, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let shim = install_askpass_shim(&dir.join("bin"), &board).unwrap();
        verify_askpass(&shim).expect("the credential path answers");
        let out = std::process::Command::new(&shim)
            .arg("Password for 'https://x-access-token@github.com': ")
            .output()
            .unwrap();
        let seen = String::from_utf8_lossy(&out.stderr);
        assert!(seen.contains("git-askpass"), "{seen}");
        assert!(seen.contains("Password for"), "{seen}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other half of gh#233: a credential path that cannot answer says so,
    /// rather than being something the caller has to infer from a failed push.
    #[test]
    #[cfg(unix)]
    fn a_credential_path_that_cannot_answer_is_an_error_with_the_reason_in_it() {
        let dir = scratch("askpass-broken");
        // Nothing there at all — the gh#233 shape, where git said only
        // "cannot exec".
        let err = verify_askpass(&dir.join("missing"))
            .expect_err("a missing helper verified")
            .to_string();
        assert!(err.contains("running the askpass helper"), "{err}");

        // There, but pointed at a binary that is not comet-board.
        let board = dir.join("comet-board");
        std::fs::write(&board, "#!/bin/sh\necho 'unknown subcommand' >&2\nexit 2\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&board, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let shim = install_askpass_shim(&dir.join("bin"), &board).unwrap();
        let err = verify_askpass(&shim)
            .expect_err("a helper that exits 2 verified")
            .to_string();
        assert!(err.contains("unknown subcommand"), "{err}");

        // There, running, and answering something git would send as a
        // username.
        std::fs::write(&board, "#!/bin/sh\necho hello\n").unwrap();
        let err = verify_askpass(&shim)
            .expect_err("a helper answering rubbish verified")
            .to_string();
        assert!(err.contains("\"hello\""), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A helper can answer the fixed username while the configured board can
    /// no longer mint a password. The dispatch preflight must exercise that
    /// second half, and a blank line is not a credential.
    #[test]
    #[cfg(unix)]
    fn the_push_credential_check_rejects_an_empty_secret_answer() {
        let dir = scratch("askpass-empty-secret");
        let board = dir.join("comet-board");
        std::fs::write(
            &board,
            "#!/bin/sh\ncase \"$2\" in Username*) echo x-access-token ;; *) echo ' ' ;; esac\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&board, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let shim = install_askpass_shim(&dir.join("bin"), &board).unwrap();
        verify_askpass(&shim).expect("the username half answers");
        let paths = Paths {
            config_dir: dir.join("config"),
            state_dir: dir.join("state"),
        };
        let error = verify_push_credential(
            &shim,
            "o/r",
            &paths,
            comet_proto::GithubPushContract {
                contents_write: true,
                workflows_write: true,
            },
        )
        .expect_err("an empty secret answer verified")
        .to_string();
        assert!(error.contains("returned no credential"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// gh#301, the write side: an installed shim is exec'able *as installed*,
    /// with no second step to wait for. Everything here holds the moment
    /// `install_askpass_shim` returns, because the next thing every caller does
    /// is exec what it returned.
    #[test]
    #[cfg(unix)]
    fn the_shim_is_complete_and_executable_the_moment_it_is_installed() {
        use std::os::unix::fs::PermissionsExt;

        let dir = scratch("shim-write");
        let bin = dir.join("bin");
        let board = dir.join("comet-board");
        let shim = install_askpass_shim(&bin, &board).expect("shim installed");

        let mode = std::fs::metadata(&shim).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "the shim is not executable as installed");
        assert_eq!(
            std::fs::read_to_string(&shim).unwrap(),
            askpass_shim_script(&board),
            "the shim's contents are not all there"
        );

        // Rewritten in place, which is what every dispatch does, and with the
        // same guarantees the second time — including no temp file left in a
        // directory that goes on an agent's PATH.
        let moved = dir.join("elsewhere").join("comet-board");
        let again = install_askpass_shim(&bin, &moved).expect("shim reinstalled");
        assert_eq!(again, shim);
        assert_eq!(
            std::fs::read_to_string(&shim).unwrap(),
            askpass_shim_script(&moved)
        );
        let left: Vec<_> = std::fs::read_dir(&bin)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n != ASKPASS_SHIM)
            .collect();
        assert!(left.is_empty(), "temp files left beside the shim: {left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// gh#301, the exec side. The write side cannot close a handle a sibling
    /// thread's fork already copied, so the check waits that handle out instead
    /// of failing the run for it.
    ///
    /// The writer here is deliberate rather than raced: a `File` open for
    /// writing on the shim, closed shortly after. On Linux that is exactly what
    /// CI hit — the exec is refused with `ETXTBSY` until the handle goes — and
    /// the assertion is that the check answers by waiting. macOS does not refuse
    /// the exec at all, which is why this only ever failed on CI, and there the
    /// test asserts the harmless half: an open handle is not an error either.
    #[test]
    #[cfg(unix)]
    fn the_check_waits_out_a_shim_that_something_still_has_open_for_writing() {
        let dir = scratch("shim-busy");
        let board = dir.join("comet-board");
        std::fs::write(&board, "#!/bin/sh\necho x-access-token\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&board, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let shim = install_askpass_shim(&dir.join("bin"), &board).unwrap();

        // No truncate: the script stays as installed, and all this does is put
        // a writer on its inode.
        let held = std::fs::OpenOptions::new().write(true).open(&shim).unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            drop(held);
        });

        verify_askpass(&shim).expect("the check gave up on a handle that was about to close");
        releaser.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What the shell says when the `exec` inside the shim is the refused one,
    /// against what a helper failing for its own reasons says. Only the first
    /// is worth waiting on, and nothing else may be mistaken for it.
    #[test]
    fn a_shell_reporting_etxtbsy_is_told_from_a_helper_that_simply_failed() {
        // dash and bash, both of which the shim can be run by.
        assert!(exec_said_busy(
            b"/state/bin/comet-board: 6: exec: /state/bin/comet-board: Text file busy\n"
        ));
        assert!(exec_said_busy(
            b"sh: line 6: /state/bin/comet-board: Text file busy\n"
        ));
        // Not this — the gh#233 shape, which has to stay an error.
        assert!(!exec_said_busy(b"sh: 1: comet-board: not found\n"));
        assert!(!exec_said_busy(b"unknown subcommand: git-askpass\n"));
        assert!(!exec_said_busy(b""));
    }

    /// And the wait ends. A dispatch must not hang behind a shim that is busy
    /// for a reason no waiting will fix; [`EXEC_BUSY_BUDGET`] is what bounds it.
    ///
    /// Linux only: it is the only platform that refuses the exec, so it is the
    /// only one with anything to bound.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_shim_that_stays_busy_is_reported_rather_than_waited_on_forever() {
        let dir = scratch("shim-busy-forever");
        let board = dir.join("comet-board");
        std::fs::write(&board, "#!/bin/sh\necho x-access-token\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&board, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let shim = install_askpass_shim(&dir.join("bin"), &board).unwrap();
        let _held = std::fs::OpenOptions::new().write(true).open(&shim).unwrap();

        let start = std::time::Instant::now();
        let err = verify_askpass(&shim)
            .expect_err("a permanently busy shim verified")
            .to_string();
        assert!(err.contains("running the askpass helper"), "{err}");
        assert!(
            start.elapsed() < EXEC_BUSY_BUDGET * 10,
            "the check waited {:?}, well past its budget",
            start.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
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
    fn the_gh_shim_mints_per_invocation_and_refuses_ambient_credentials() {
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
        assert!(script.contains("unset GH_TOKEN GITHUB_TOKEN"), "{script}");
        assert!(
            script.contains("could not mint the board credential"),
            "{script}"
        );
        assert!(script.contains("GH_HOST=$GH_HOST"), "{script}");
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
    ///
    /// Exec'd through [`exec_waiting_out_busy`], for the reason
    /// [`verify_askpass`] gives at length: this test writes three files and
    /// runs them, inside a `cargo test` binary whose other threads are forking
    /// the whole time, and on Linux a child that has not reached its own `exec`
    /// yet holds those inodes open for writing. gh#385 was this test failing
    /// that way on CI — never on macOS, which does not enforce `ETXTBSY`, and
    /// never on demand. Writing more carefully cannot fix it: [`install_shim`]
    /// already renames a closed file into place, and the handle the kernel
    /// objects to is a *copy* of one this thread had already dropped.
    #[test]
    #[cfg(unix)]
    fn the_gh_shim_fails_instead_of_falling_back_when_nothing_can_be_minted() {
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
        let out = exec_waiting_out_busy(
            std::process::Command::new(bin.join("gh"))
                .arg("pr")
                .env(ASKPASS_REPO_ENV, "o/r")
                .env("GH_TOKEN", "ambient-box-token")
                .env_remove("GITHUB_TOKEN"),
        )
        .unwrap();
        assert!(!out.status.success());
        assert!(out.stdout.is_empty(), "ambient gh ran: {out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("could not mint the board credential")
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
