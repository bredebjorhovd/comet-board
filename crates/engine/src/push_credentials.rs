//! Handing the board's GitHub credential to a dispatched agent (gh#68).
//!
//! comet-board built the mechanism with #58 — `GIT_ASKPASS` at `comet-board
//! git-askpass`, minting an App installation token at the moment git asks —
//! and left it with no caller: a dispatched agent pushed with whatever git
//! credentials the box user had. That works on the Mac somebody set up by
//! hand and fails on a clean headless box, where there is no keychain, no
//! stored https credential, and no `gh auth login` anybody can do
//! interactively. This is the caller.
//!
//! Two things reach the harness child, and neither of them is a token:
//!
//! - the askpass wiring, for `git push`;
//! - a directory holding a `gh` wrapper, prepended to the child's PATH, for
//!   `gh pr create`. `gh` reads `GH_TOKEN` once at startup and a token lives an
//!   hour, so exporting one at spawn would hand a three-hour run an expired
//!   credential exactly when it goes to open its pull request. The wrapper
//!   mints per invocation instead.
//!
//! A chat the board did not dispatch is untouched. A board-dispatched GitHub
//! chat is different: missing credentials, helpers, or `gh` refuse the run so
//! it can never silently deliver through the box user's ambient login.

use comet_board::config::{Credentials, GithubAuth, Paths};
use comet_board::git_credentials;
use std::path::{Path, PathBuf};

use crate::workspace_host::WorkspaceHost;

/// Where the `gh` shim is installed, under the board's state dir. One per
/// device rather than one per run: its contents depend on nothing but the two
/// binaries' paths, and PATH entries are cheap while directories are not.
pub(crate) const SHIM_DIR: &str = "bin";

/// The device's board credential, as something a run can be pointed at.
pub struct PushCredentials {
    /// The board's own directories — passed to the helper explicitly, so a
    /// helper process started with none of the engine's arguments reads the
    /// same `.env` the engine's board loop does.
    paths: Paths,
    /// The `comet-board` binary the helpers run as. `None` makes a
    /// board-dispatched GitHub run undeliverable: the handoff may never fall
    /// back to the box's credential helper or terminal prompting.
    board_exe: Option<PathBuf>,
}

impl PushCredentials {
    /// Resolve what this device can offer. Cheap and infallible; missing pieces
    /// become a fatal handoff error when a board-dispatched GitHub run asks.
    pub fn detect(paths: Paths) -> Self {
        Self::with_board_exe(paths, git_credentials::resolve_board_exe())
    }

    /// As [`PushCredentials::detect`], with the helper binary already known —
    /// the seam for a caller (a test, a packaged build) that can say where
    /// `comet-board` is instead of going looking for it.
    pub fn with_board_exe(paths: Paths, board_exe: Option<PathBuf>) -> Self {
        Self { paths, board_exe }
    }

    /// The credentials for a board-dispatched run pushing to `repo`.
    ///
    /// Failure is fatal: a chat carrying `push_repo` is not an ordinary chat
    /// that may fall back to the box user's ambient git/gh credentials. This
    /// verifies the same askpass and gh-wrapper handoff the child receives.
    ///
    /// `chat` is the run this is for, recorded in the ledger so the settle can
    /// ask afterwards whether the credential it handed over was the one that
    /// pushed (gh#233).
    pub fn for_repo(
        &self,
        repo: &str,
        contract: comet_proto::GithubPushContract,
        chat: Option<&str>,
    ) -> anyhow::Result<comet_harness::PushCredentials> {
        // Re-read per run rather than at boot: an operator who drops a PEM on
        // the box and restarts the board loop should not also have to restart
        // the engine.
        self.prepare_for_repo_with(
            repo,
            Credentials::load(&self.paths).github_auth(),
            contract,
            chat,
            true,
        )
    }

    /// Exercise the exact handoff before the board creates an attempt, without
    /// recording that a run received credentials when no run exists yet.
    pub fn verify_for_repo(
        &self,
        repo: &str,
        contract: comet_proto::GithubPushContract,
    ) -> anyhow::Result<()> {
        self.prepare_for_repo_with(
            repo,
            Credentials::load(&self.paths).github_auth(),
            contract,
            None,
            false,
        )
        .map(|_| ())
    }

    /// Resolve the push state for a chat, lazily upgrading rows dispatched
    /// before `GithubPushContract` existed. The old row proves only repository
    /// ownership, so recovery persists the minimum useful promise (contents
    /// write, no workflow write) after the current board handoff proves it.
    /// Failure leaves the row untouched and therefore fail-closed.
    pub fn resolve_chat_state(
        &self,
        workspace: &WorkspaceHost,
        chat_id: &str,
    ) -> anyhow::Result<Option<comet_proto::GithubPushState>> {
        match workspace.github_push_state(chat_id) {
            Ok(state) => Ok(state),
            Err(original) => {
                let Some(repo) = workspace.legacy_github_push_repo(chat_id)? else {
                    return Err(anyhow::anyhow!(original.to_string()));
                };
                let contract = comet_proto::GithubPushContract {
                    contents_write: true,
                    workflows_write: false,
                };
                self.verify_for_repo(&repo, contract).map_err(|error| {
                    anyhow::anyhow!(
                        "legacy board chat {chat_id} could not establish a replacement push contract for {repo}: {error:#}"
                    )
                })?;
                let recovered = comet_proto::GithubPushState { repo, contract };
                if workspace.upgrade_legacy_github_push_state(chat_id, &recovered)? {
                    tracing::info!(
                        chat = chat_id,
                        repo = %recovered.repo,
                        "upgraded legacy board chat to a proven contents-write push contract"
                    );
                    return Ok(Some(recovered));
                }
                // A remote write won the probe race. Re-read its authoritative
                // result; never overwrite it with the stale proof above.
                workspace
                    .github_push_state(chat_id)
                    .map_err(|error| anyhow::anyhow!(error.to_string()))
            }
        }
    }

    #[cfg(test)]
    fn for_repo_with(
        &self,
        repo: &str,
        auth: GithubAuth,
        contract: comet_proto::GithubPushContract,
        chat: Option<&str>,
    ) -> anyhow::Result<comet_harness::PushCredentials> {
        self.prepare_for_repo_with(repo, auth, contract, chat, true)
    }

    fn prepare_for_repo_with(
        &self,
        repo: &str,
        auth: GithubAuth,
        contract: comet_proto::GithubPushContract,
        chat: Option<&str>,
        record_handoff: bool,
    ) -> anyhow::Result<comet_harness::PushCredentials> {
        if repo.is_empty() {
            anyhow::bail!("a board-dispatched GitHub chat has no repository to authenticate for");
        }
        if auth == GithubAuth::None {
            return self.unusable(
                repo,
                chat,
                "no GitHub credential configured — set GITHUB_TOKEN, or GITHUB_APP_ID and GITHUB_APP_PRIVATE_KEY_PATH, in the board's .env",
            );
        }
        let Some(exe) = &self.board_exe else {
            return self.unusable(
                repo,
                chat,
                "the comet-board binary is not on this device — install it beside the engine, on PATH, or name it with COMET_BOARD_EXECUTABLE",
            );
        };
        // Everything above is a device that was never asked to do this. What
        // follows is a device that *was*, and the two must not read the same:
        // an operator who configured a credential and a binary is owed a noise
        // when the path between them does not work.
        let askpass = match self.askpass_shim(exe, repo, contract) {
            Ok(path) => path,
            Err(err) => {
                let err = format!("{err:#}");
                tracing::error!(
                    repo,
                    error = %err,
                    "the board's credential path does not work on this device — no dispatched \
                     agent can push with it, and an agent that finds its own way round is the \
                     failure gh#68 exists to prevent"
                );
                comet_board::credential_ledger::unusable(&self.paths, repo, chat, &err);
                anyhow::bail!("the board's credential handoff for {repo} failed: {err}");
            }
        };
        let bin_dir = match self.tool_shims(exe, &askpass, repo, contract, chat) {
            Ok(dir) => dir,
            Err(error) => {
                let err = format!("{error:#}");
                comet_board::credential_ledger::unusable(&self.paths, repo, chat, &err);
                anyhow::bail!("the board's credential handoff for {repo} failed: {err}");
            }
        };
        if record_handoff {
            comet_board::credential_ledger::handed(&self.paths, repo, chat);
        }
        Ok(comet_harness::PushCredentials {
            env: git_credentials::agent_env_with_contract(&askpass, repo, &self.paths, contract),
            bin_dir: Some(bin_dir),
        })
    }

    fn unusable<T>(&self, repo: &str, chat: Option<&str>, message: &str) -> anyhow::Result<T> {
        tracing::error!(repo, error = message, "board credential handoff refused");
        comet_board::credential_ledger::unusable(&self.paths, repo, chat, message);
        anyhow::bail!("the board's credential handoff for {repo} failed: {message}")
    }

    /// Install the askpass shim and prove it answers, before a run is given it
    /// (gh#233).
    ///
    /// The check is not paranoia about our own file writing: it is the one
    /// place the *whole* path is exercised — exec the shim, reach a
    /// `comet-board`, have it understand `git-askpass`, read the answer off
    /// stdout. gh#233 was every one of those working in a unit test and none
    /// of them working under git, because the thing being tested was the shape
    /// of a string rather than a process anybody had run.
    ///
    /// Doing it per run rather than once at boot costs a `fork` on a code path
    /// that already cuts a worktree, and it means a box that is repaired under
    /// a running engine is repaired without a restart.
    fn askpass_shim(
        &self,
        board_exe: &Path,
        repo: &str,
        contract: comet_proto::GithubPushContract,
    ) -> anyhow::Result<PathBuf> {
        let dir = self.paths.state_dir.join(SHIM_DIR);
        let shim = git_credentials::install_askpass_shim(&dir, board_exe)?;
        git_credentials::verify_askpass(&shim)?;
        git_credentials::verify_push_credential(&shim, repo, &self.paths, contract)?;
        Ok(shim)
    }

    /// The PATH entry carrying the `gh` wrapper, when there is a `gh` to wrap.
    ///
    /// This is part of the required handoff: allowing a missing wrapper would
    /// make `gh` silently use the box user's ambient login.
    fn tool_shims(
        &self,
        board_exe: &Path,
        askpass: &Path,
        repo: &str,
        contract: comet_proto::GithubPushContract,
        chat: Option<&str>,
    ) -> anyhow::Result<PathBuf> {
        let Some(chat) = chat else {
            let dir = self.paths.state_dir.join(SHIM_DIR);
            let gh = git_credentials::resolve_gh(Some(&dir))
                .ok_or_else(|| anyhow::anyhow!("the gh binary is not installed on this device"))?;
            return git_credentials::install_gh_shim(&dir, board_exe, &gh);
        };
        if chat.is_empty() || !chat.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            anyhow::bail!("a credentialed run has an unsafe chat id");
        }
        let dir = self.paths.state_dir.join(SHIM_DIR).join(chat);
        let gh = git_credentials::resolve_gh(Some(&dir))
            .ok_or_else(|| anyhow::anyhow!("the gh binary is not installed on this device"))?;
        git_credentials::install_gh_shim(&dir, board_exe, &gh)?;
        let git = git_credentials::resolve_git(Some(&dir))
            .ok_or_else(|| anyhow::anyhow!("git is not installed on this device"))?;
        git_credentials::install_git_shim(
            &dir, &git, askpass, repo, &self.paths, contract, chat,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_in(dir: &Path) -> Paths {
        // Scratch dirs, not the device's board. `Paths::under` used to honour
        // the COMET_BOARD_* overrides, which tests must not depend on; since
        // gh#190 it cannot, so this is the plain derivation again.
        Paths::under(dir).expect("board dirs")
    }

    fn scratch(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("comet-push-{name}-"))
            .tempdir()
            .unwrap()
    }

    /// A stand-in `comet-board` that answers `git-askpass` the way the real one
    /// does. Since gh#233 the resolver *runs* what it is about to hand over, so
    /// a path that merely looks plausible is no longer enough to test with —
    /// which is the point.
    #[cfg(unix)]
    fn fake_board(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let exe = dir.join("comet-board");
        std::fs::write(
            &exe,
            "#!/bin/sh\n\
             [ \"$1\" = git-askpass ] || exit 2\n\
             echo x-access-token\n",
        )
        .unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        exe
    }

    /// A resolver that has everything it needs, pointed at scratch dirs. The
    /// credential is passed to `for_repo_with` rather than read from the
    /// environment: these tests must pass on a box that exports GITHUB_TOKEN
    /// and on one that does not.
    fn resolver(dir: &Path) -> PushCredentials {
        PushCredentials {
            paths: paths_in(dir),
            board_exe: Some(fake_board(dir)),
        }
    }

    fn app() -> GithubAuth {
        GithubAuth::App {
            app_id: "1234".into(),
            key_path: PathBuf::from("/keys/board.pem"),
        }
    }

    fn contract() -> comet_proto::GithubPushContract {
        comet_proto::GithubPushContract {
            contents_write: true,
            workflows_write: true,
        }
    }

    /// A board-dispatched GitHub run without a board credential is refused.
    #[test]
    fn a_device_with_no_github_credential_refuses_the_run() {
        let dir = scratch("nocred");
        assert!(
            resolver(dir.path())
                .for_repo_with("o/r", GithubAuth::None, contract(), None)
                .is_err()
        );
    }

    #[test]
    fn a_device_with_no_comet_board_binary_refuses_the_run() {
        let dir = scratch("nobin");
        let mut creds = resolver(dir.path());
        creds.board_exe = None;
        assert!(creds.for_repo_with("o/r", app(), contract(), None).is_err());
    }

    /// The whole point: askpass wiring, the board's own directories, and not
    /// one byte of credential.
    #[test]
    fn the_environment_points_at_the_helper_and_carries_no_token() {
        let dir = scratch("env");
        let resolver = resolver(dir.path());
        let push = resolver
            .for_repo_with("o/r", app(), contract(), Some("chat-1"))
            .expect("credentials");
        let env: std::collections::BTreeMap<_, _> = push.env.into_iter().collect();
        // A path, and one that names a file — since gh#233, GIT_ASKPASS
        // carries nothing a shell would have to take apart.
        let askpass = env.get("GIT_ASKPASS").expect("GIT_ASKPASS").clone();
        assert_eq!(
            askpass,
            resolver
                .paths
                .state_dir
                .join(SHIM_DIR)
                .join(git_credentials::ASKPASS_SHIM)
                .display()
                .to_string()
        );
        assert!(Path::new(&askpass).is_file(), "no helper at {askpass}");
        assert_eq!(
            env.get("COMET_BOARD_ASKPASS_REPO").map(String::as_str),
            Some("o/r")
        );
        assert_eq!(
            env.get("COMET_BOARD_CONFIG_DIR").map(String::as_str),
            Some(dir.path().join("board").display().to_string().as_str())
        );
        assert!(
            !env.values()
                .any(|v| v.contains("ghs_") || v.contains("ghp_")),
            "a token reached the environment: {env:?}"
        );
        // And the run is on the record as having been given it, which is what
        // makes a later "nobody ever asked" mean something (gh#233).
        assert!(comet_board::credential_ledger::for_chat(&resolver.paths, "chat-1").handed);
    }

    /// An empty repo is a chat the board did not dispatch (or one whose space
    /// has no GitHub remote); it must not reach the helper, which would then
    /// mint for whatever the last run left in its environment.
    #[test]
    fn no_repo_means_no_credentials() {
        let dir = scratch("norepo");
        assert!(
            resolver(dir.path())
                .for_repo_with("", app(), contract(), None)
                .is_err()
        );
    }

    /// gh#233. A device that was configured to hand out the board's credential
    /// and cannot is not the same as one that was never asked to: it is a
    /// fault, it is recorded as one, and the run is not given a credential path
    /// that does not work.
    #[test]
    #[cfg(unix)]
    fn a_credential_path_that_does_not_work_is_refused_and_recorded() {
        let dir = scratch("broken");
        let mut creds = resolver(dir.path());
        // Installed, resolvable, and reaching nothing — a payload that shipped
        // the engine without the CLI beside it.
        creds.board_exe = Some(dir.path().join("not-installed"));
        assert!(
            creds
                .for_repo_with("o/r", app(), contract(), Some("chat-1"))
                .is_err()
        );

        let record = comet_board::credential_ledger::for_chat(&creds.paths, "chat-1");
        assert!(!record.handed, "a broken path was handed to a run");
        assert!(
            record.unsanctioned(),
            "a broken path left nothing for the settle to notice: {record:?}"
        );
        let failure = record.last_failure().expect("a recorded failure");
        assert!(
            failure.summary().contains("askpass helper"),
            "{}",
            failure.summary()
        );
    }

    /// The shim is installed executable, names both binaries, and takes the
    /// repo from the environment the run already carries.
    #[test]
    fn the_gh_shim_is_written_executable_and_late_minting() {
        let dir = scratch("install");
        let creds = resolver(dir.path());
        let bin = creds.paths.state_dir.join(SHIM_DIR);
        let installed = git_credentials::install_gh_shim(
            &bin,
            Path::new("/opt/comet/comet-board"),
            Path::new("/usr/bin/gh"),
        )
        .expect("shim installed");
        assert_eq!(installed, bin);
        let script = std::fs::read_to_string(bin.join("gh")).unwrap();
        assert!(
            script.contains("'/opt/comet/comet-board' gh-token"),
            "{script}"
        );
        assert!(script.contains("exec '/usr/bin/gh' \"$@\""), "{script}");
        assert!(script.contains("COMET_BOARD_ASKPASS_REPO"), "{script}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(bin.join("gh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "the shim is not executable: {mode:o}");
        }
    }
}
