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
//! Everything is conditional, and the fallback is always "what happened
//! before". No board credential configured, no `comet-board` on the box, no
//! `gh` installed, no repo on the chat: the child is spawned untouched and the
//! agent pushes as the box user, exactly as it did before this change.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use comet_board::config::{Credentials, GithubAuth, Paths};
use comet_board::git_credentials;

/// Where the `gh` shim is installed, under the board's state dir. One per
/// device rather than one per run: its contents depend on nothing but the two
/// binaries' paths, and PATH entries are cheap while directories are not.
const SHIM_DIR: &str = "bin";

/// The device's board credential, as something a run can be pointed at.
pub struct PushCredentials {
    /// The board's own directories — passed to the helper explicitly, so a
    /// helper process started with none of the engine's arguments reads the
    /// same `.env` the engine's board loop does.
    paths: Paths,
    /// The `comet-board` binary the helpers run as. `None` disables the whole
    /// thing: pointing `GIT_ASKPASS` at a binary that is not there would break
    /// pushes that work today, since the same environment switches off the
    /// box's credential helper and terminal prompting.
    board_exe: Option<PathBuf>,
    /// Whether the operator has already been told this device cannot do it.
    /// The alternative is one warning per run, forever.
    warned: Mutex<bool>,
}

impl PushCredentials {
    /// Resolve what this device can offer. Cheap and infallible — every part
    /// that can be missing is optional, and what is missing is reported the
    /// first time a run actually wants it.
    pub fn detect(paths: Paths) -> Self {
        Self::with_board_exe(paths, git_credentials::resolve_board_exe())
    }

    /// As [`PushCredentials::detect`], with the helper binary already known —
    /// the seam for a caller (a test, a packaged build) that can say where
    /// `comet-board` is instead of going looking for it.
    pub fn with_board_exe(paths: Paths, board_exe: Option<PathBuf>) -> Self {
        Self {
            paths,
            board_exe,
            warned: Mutex::new(false),
        }
    }

    /// The credentials for a run pushing to `repo`, or `None` when this device
    /// cannot authenticate for it and the agent should keep using the box's
    /// own git credentials.
    pub fn for_repo(&self, repo: &str) -> Option<comet_harness::PushCredentials> {
        // Re-read per run rather than at boot: an operator who drops a PEM on
        // the box and restarts the board loop should not also have to restart
        // the engine.
        self.for_repo_with(repo, Credentials::load(&self.paths).github_auth())
    }

    fn for_repo_with(
        &self,
        repo: &str,
        auth: GithubAuth,
    ) -> Option<comet_harness::PushCredentials> {
        if repo.is_empty() {
            return None;
        }
        if auth == GithubAuth::None {
            self.warn_once(
                "no GitHub credential configured — dispatched agents push with this \
                 device's own git credentials (set GITHUB_TOKEN, or GITHUB_APP_ID and \
                 GITHUB_APP_PRIVATE_KEY_PATH, in the board's .env)",
            );
            return None;
        }
        let Some(exe) = &self.board_exe else {
            self.warn_once(
                "the comet-board binary is not on this device — dispatched agents push \
                 with its own git credentials (install it beside the engine, on PATH, or \
                 name it with COMET_BOARD_EXECUTABLE)",
            );
            return None;
        };
        Some(comet_harness::PushCredentials {
            env: git_credentials::agent_env(exe, repo, &self.paths),
            bin_dir: self.gh_shim(exe),
        })
    }

    /// The PATH entry carrying the `gh` wrapper, when there is a `gh` to wrap.
    ///
    /// A failure here is not a failure of the dispatch: git still pushes
    /// through askpass, and an agent with no `gh` was never going to run it.
    fn gh_shim(&self, board_exe: &Path) -> Option<PathBuf> {
        let dir = self.paths.state_dir.join(SHIM_DIR);
        let gh = git_credentials::resolve_gh(Some(&dir))?;
        match git_credentials::install_gh_shim(&dir, board_exe, &gh) {
            Ok(dir) => Some(dir),
            Err(err) => {
                tracing::warn!(error = %err, "gh shim not installed — `gh` will use the box's own login");
                None
            }
        }
    }

    fn warn_once(&self, message: &str) {
        let mut warned = self.warned.lock().unwrap_or_else(PoisonError::into_inner);
        if std::mem::replace(&mut *warned, true) {
            return;
        }
        tracing::warn!("{message}");
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

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("comet-push-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A resolver that has everything it needs, pointed at scratch dirs. The
    /// credential is passed to `for_repo_with` rather than read from the
    /// environment: these tests must pass on a box that exports GITHUB_TOKEN
    /// and on one that does not.
    fn resolver(dir: &Path) -> PushCredentials {
        PushCredentials {
            paths: paths_in(dir),
            board_exe: Some(PathBuf::from("/opt/comet/comet-board")),
            warned: Mutex::new(false),
        }
    }

    fn app() -> GithubAuth {
        GithubAuth::App {
            app_id: "1234".into(),
            key_path: PathBuf::from("/keys/board.pem"),
        }
    }

    /// The gate that keeps a box without a board credential working the way it
    /// always has: the child is spawned with nothing added, so the agent's
    /// push uses whatever git the box already has.
    #[test]
    fn a_device_with_no_github_credential_offers_nothing() {
        let dir = scratch("nocred");
        assert!(
            resolver(&dir)
                .for_repo_with("o/r", GithubAuth::None)
                .is_none()
        );
    }

    #[test]
    fn a_device_with_no_comet_board_binary_offers_nothing() {
        let dir = scratch("nobin");
        let mut creds = resolver(&dir);
        creds.board_exe = None;
        assert!(creds.for_repo_with("o/r", app()).is_none());
    }

    /// The whole point: askpass wiring, the board's own directories, and not
    /// one byte of credential.
    #[test]
    fn the_environment_points_at_the_helper_and_carries_no_token() {
        let dir = scratch("env");
        let push = resolver(&dir)
            .for_repo_with("o/r", app())
            .expect("credentials");
        let env: std::collections::BTreeMap<_, _> = push.env.into_iter().collect();
        assert_eq!(
            env.get("GIT_ASKPASS").map(String::as_str),
            Some("'/opt/comet/comet-board' git-askpass")
        );
        assert_eq!(
            env.get("COMET_BOARD_ASKPASS_REPO").map(String::as_str),
            Some("o/r")
        );
        assert_eq!(
            env.get("COMET_BOARD_CONFIG_DIR").map(String::as_str),
            Some(dir.join("board").display().to_string().as_str())
        );
        assert!(
            !env.values()
                .any(|v| v.contains("ghs_") || v.contains("ghp_")),
            "a token reached the environment: {env:?}"
        );
    }

    /// An empty repo is a chat the board did not dispatch (or one whose space
    /// has no GitHub remote); it must not reach the helper, which would then
    /// mint for whatever the last run left in its environment.
    #[test]
    fn no_repo_means_no_credentials() {
        let dir = scratch("norepo");
        assert!(resolver(&dir).for_repo_with("", app()).is_none());
    }

    /// The shim is installed executable, names both binaries, and takes the
    /// repo from the environment the run already carries.
    #[test]
    fn the_gh_shim_is_written_executable_and_late_minting() {
        let dir = scratch("install");
        let creds = resolver(&dir);
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
