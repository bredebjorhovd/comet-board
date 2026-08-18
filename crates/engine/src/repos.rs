//! Repos — this device's git repositories, branches, worktrees, and the folder
//! browser (feature-inventory §3.5; port of comet's `repos.ts` + `folder-lister.ts`).
//!
//! Repos are device-local (paths differ per machine), so the known set is a plain
//! JSON list (`{data_dir}/repos.json`) — no sync. Existing repos can live anywhere
//! the user points us; cloned/created ones land in `{data_dir}/repos`. Worktrees are
//! created under `~/.comet-native/worktrees/<repoName>/<worktreeName>` (NOT the data
//! dir — worktrees are user-facing working checkouts), with an auto-generated name +
//! matching `comet/<name>` branch. `COMET_WORKTREES_DIR` overrides the root.
//!
//! All git access is via subprocess (`tokio::process`) — never libgit2.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

use comet_proto::{FolderEntry, FolderListing, Repo, RepoRef, Worktree};

use crate::EngineError;

/// Existence probe timeout for user-chosen / remembered paths, which can point at
/// dead network mounts where a bare `stat` hangs for minutes.
const PATH_EXISTS_TIMEOUT: Duration = Duration::from_secs(2);
/// Hard wall-clock ceiling for a folder listing (the walk runs in a disposable
/// blocking task; on expiry the caller unblocks and the task is abandoned).
const FOLDER_LIST_TIMEOUT: Duration = Duration::from_secs(6);
/// Cap on returned folder entries (bounds response size).
const FOLDER_LIST_MAX_ENTRIES: usize = 500;

const ADJECTIVES: &[&str] = &[
    "swift", "calm", "bright", "bold", "keen", "brave", "clever", "lucky", "quiet", "warm", "cool",
    "sharp", "gentle", "vivid", "amber", "cobalt",
];
const NOUNS: &[&str] = &[
    "otter", "harbor", "falcon", "cedar", "meadow", "comet", "delta", "ember", "lynx", "maple",
    "onyx", "quartz", "raven", "summit", "willow", "aspen",
];

/// Canonical identity shared by every chat operating in this exact worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutIdentity {
    /// `sha256(deviceId ‖ NUL ‖ canonical git dir)` — device-scoped, path-stable.
    pub id: String,
    /// Canonical worktree root (`rev-parse --show-toplevel`, symlinks resolved).
    pub root: PathBuf,
    /// Canonical git dir (worktree-specific for linked worktrees).
    pub git_dir: PathBuf,
}

/// Best-effort home directory (the `ListFolders` default and worktree root base).
pub(crate) fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Where new worktrees live. Deliberately NOT under the backend data dir —
/// worktrees are user-facing working checkouts. `COMET_WORKTREES_DIR` overrides
/// (test isolation); empty reads as unset.
///
/// The rule itself lives in `comet_board::config` because the board reclaims
/// and reports on these directories (gh#72) and a second copy of it would mean
/// `doctor` measuring a directory nothing writes to.
fn default_worktrees_root() -> PathBuf {
    comet_board::config::worktrees_root()
}

/// How loudly a failing git command is written down (gh#317).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Noise {
    /// Failure is news: it broke something a person asked for.
    Loud,
    /// Failure is the answer to a question ("is this a repo?"), so it goes to
    /// `debug` — a probe that warned on every non-repo folder would make the
    /// warn stream useless on the day it matters.
    Probe,
}

/// The command as it goes into the log: `git <args>`, with any URL password
/// blanked.
///
/// The URL itself is kept on purpose — gh#316 was a clone that failed over
/// *which* URL it used, and a log line that omits it cannot answer that. Only
/// the `:password` half of a URL's userinfo is blanked; the board's own
/// `x-access-token@` is a username, not a secret, and reading it in the log is
/// how you tell an authenticated clone from an ambient one.
fn redacted_command(args: &[&str]) -> String {
    let mut out = String::from("git");
    for arg in args {
        out.push(' ');
        out.push_str(&loggable_url(arg));
    }
    out
}

/// `scheme://user:secret@host/…` → `scheme://user:***@host/…`. Anything that is
/// not a URL with a password comes back untouched.

struct ReposInner {
    data_dir: PathBuf,
    device_id: String,
    worktrees_root: PathBuf,
    planned_worktrees: std::sync::Mutex<HashSet<PathBuf>>,
}

#[derive(Clone)]
pub struct Repos {
    inner: std::sync::Arc<ReposInner>,
}

impl Repos {
    /// Resolve mutable workspace rows only when both the owning space and the
    /// requested checkout are roots registered on this host.
    pub async fn validated_checkout_root(
        &self,
        space_path: &str,
        candidate: &str,
    ) -> Option<PathBuf> {
        let space_root = std::fs::canonicalize(space_path).ok()?;
        let candidate_root = std::fs::canonicalize(candidate).ok()?;
        for repo in self.list().await {
            let repo_root = std::fs::canonicalize(&repo.path).ok()?;
            let mut registered = vec![repo_root];
            if let Ok(refs) = self.refs(Path::new(&repo.path)).await {
                registered.extend(
                    refs.into_iter()
                        .filter_map(|reference| reference.worktree_path)
                        .filter_map(|path| std::fs::canonicalize(path).ok()),
                );
            }
            if registered.contains(&space_root) && registered.contains(&candidate_root) {
                return Some(candidate_root);
            }
        }
        None
    }

    /// `data_dir` holds `repos.json` + cloned/created repos; the worktree root
    /// comes from `$COMET_WORKTREES_DIR` or `~/.comet-native/worktrees`.
    pub fn new(data_dir: &Path, device_id: &str) -> Self {
        Self::with_worktrees_root(data_dir, device_id, default_worktrees_root())
    }

    /// Explicit worktree root (tests).
    pub fn with_worktrees_root(data_dir: &Path, device_id: &str, worktrees_root: PathBuf) -> Self {
        Self {
            inner: std::sync::Arc::new(ReposInner {
                data_dir: data_dir.to_path_buf(),
                device_id: device_id.to_string(),
                worktrees_root,
                planned_worktrees: std::sync::Mutex::new(HashSet::new()),
            }),
        }
    }

    // ── registry (repos.json) ───────────────────────────────────────────────

    fn registry_path(&self) -> PathBuf {
        self.inner.data_dir.join("repos.json")
    }

    fn load_paths(&self) -> Vec<String> {
        std::fs::read_to_string(self.registry_path())
            .ok()
            .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
            .unwrap_or_default()
    }

    fn save_paths(&self, paths: &[String]) -> Result<(), EngineError> {
        let mut seen = HashSet::new();
        let deduped: Vec<&String> = paths.iter().filter(|p| seen.insert(p.as_str())).collect();
        let json = serde_json::to_string_pretty(&deduped)
            .map_err(|e| EngineError::Other(format!("repos registry serialize: {e}")))?;
        std::fs::create_dir_all(&self.inner.data_dir)?;
        std::fs::write(self.registry_path(), json)?;
        Ok(())
    }

    fn register(&self, path: &str) -> Result<(), EngineError> {
        let mut paths = self.load_paths();
        paths.push(path.to_string());
        self.save_paths(&paths)
    }

    // ── git plumbing ────────────────────────────────────────────────────────

    /// Run `git <args>` (optionally under `cwd`), returning trimmed stdout.
    async fn git(&self, args: &[&str], cwd: Option<&Path>) -> Result<String, EngineError> {
        self.git_env(args, cwd, &[]).await
    }

    /// [`Repos::git`] for a command whose failure is an ANSWER rather than a
    /// fault: "is this a repo?", "does this branch exist?", "has it a remote?".
    /// Those fail constantly and by design, and logging them at `warn` would
    /// bury the one failure a reader is looking for (gh#317).
    async fn git_probe(&self, args: &[&str], cwd: Option<&Path>) -> Result<String, EngineError> {
        self.run_git(args, cwd, &[], Noise::Probe).await
    }

    /// [`Repos::git`], with extra environment for the child.
    ///
    /// The environment is how a credential reaches git without being written
    /// anywhere it outlives the command — see `comet_board::git_credentials`,
    /// which builds the pairs. Nothing secret is in them: they name an askpass
    /// helper, and the helper prints the token onto git's own pipe.
    async fn git_env(
        &self,
        args: &[&str],
        cwd: Option<&Path>,
        env: &[(String, String)],
    ) -> Result<String, EngineError> {
        self.run_git(args, cwd, env, Noise::Loud).await
    }

    /// The one place a `git` child is spawned — and the one place its failure is
    /// written down.
    ///
    /// Every git failure is traced here, with git's WHOLE stderr, because the
    /// caller's copy of it goes to a screen that has room for one line (gh#317).
    /// The half-truth on that line cost an hour of live reproduction across
    /// three watchers to diagnose (gh#316) for want of the two lines git had
    /// already written and nobody kept.
    async fn run_git(
        &self,
        args: &[&str],
        cwd: Option<&Path>,
        env: &[(String, String)],
        noise: Noise,
    ) -> Result<String, EngineError> {
        let mut cmd = tokio::process::Command::new("git");
        cmd.args(args);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.stdin(std::process::Stdio::null());
        let output = cmd.output().await.map_err(|e| {
            let command = redacted_command(args);
            tracing::warn!(command, error = %e, "git spawn failed");
            EngineError::Other(format!("git spawn failed: {e}"))
        })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = stderr.trim();
            let command = redacted_command(args);
            let cwd = cwd.map(|p| p.display().to_string());
            // The whole of git's stderr, every line of it: the last line is the
            // diagnosis, and it is exactly the one a footer drops.
            match noise {
                Noise::Loud => tracing::warn!(
                    command,
                    cwd,
                    status = %output.status,
                    stderr = message,
                    "git failed"
                ),
                Noise::Probe => tracing::debug!(
                    command,
                    cwd,
                    status = %output.status,
                    stderr = message,
                    "git probe failed"
                ),
            }
            return Err(EngineError::Other(if message.is_empty() {
                format!(
                    "git {} failed ({})",
                    args.first().unwrap_or(&"?"),
                    output.status
                )
            } else {
                format!("git: {message}")
            }));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Async existence probe with a timeout: a wedged network mount just reads
    /// as "gone" instead of hanging every caller.
    async fn path_exists(path: &Path) -> bool {
        let path = path.to_path_buf();
        matches!(
            tokio::time::timeout(PATH_EXISTS_TIMEOUT, tokio::fs::metadata(path)).await,
            Ok(Ok(_))
        )
    }

    /// Is `path` inside a git work tree? (Also the SpacesSync git-presence probe.)
    pub async fn is_repo(&self, path: &Path) -> bool {
        matches!(
            self.git_probe(&["rev-parse", "--is-inside-work-tree"], Some(path)).await,
            Ok(out) if out == "true"
        )
    }

    /// The branch currently checked out at a repo/worktree path (`"HEAD"` when detached).
    pub async fn current_branch(&self, path: &Path) -> Result<String, EngineError> {
        let branch = self.git(&["branch", "--show-current"], Some(path)).await?;
        Ok(if branch.is_empty() {
            "HEAD".to_string()
        } else {
            branch
        })
    }

    /// The absolute Git `HEAD` file for event-driven external branch reconciliation.
    pub async fn git_head_path(&self, path: &Path) -> Result<PathBuf, EngineError> {
        let git_dir = self
            .git(&["rev-parse", "--absolute-git-dir"], Some(path))
            .await?;
        Ok(PathBuf::from(git_dir).join("HEAD"))
    }

    /// The repository a checkout belongs to — the primary work tree's root,
    /// asked of the checkout itself (gh#425).
    ///
    /// A linked worktree's *common* git dir is the primary checkout's `.git`,
    /// so its parent is the repo root. That is the folder `git worktree add`
    /// has to be run in, which is why forking a chat that already lives in a
    /// worktree can cut a sibling rather than nesting inside it.
    pub async fn repo_root(&self, path: &Path) -> Result<PathBuf, EngineError> {
        let common = self
            .git(
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                Some(path),
            )
            .await?;
        PathBuf::from(&common)
            .parent()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| EngineError::Other(format!("{common} has no parent repository")))
    }

    /// The commit a checkout is standing on — a fork's start point, resolved to
    /// a sha so the new worktree lands exactly where the source chat is rather
    /// than wherever a branch name has since moved to.
    pub async fn head_sha(&self, path: &Path) -> Result<String, EngineError> {
        self.git(&["rev-parse", "HEAD"], Some(path)).await
    }

    /// Canonical identity shared by every chat operating in this exact worktree:
    /// `sha256(deviceId ‖ NUL ‖ canonical git dir)`.
    pub async fn checkout_identity(&self, path: &Path) -> Result<CheckoutIdentity, EngineError> {
        let root = self
            .git(&["rev-parse", "--show-toplevel"], Some(path))
            .await?;
        let git_dir = self
            .git(
                &["rev-parse", "--path-format=absolute", "--git-dir"],
                Some(path),
            )
            .await?;
        let canonical_root = std::fs::canonicalize(&root).unwrap_or_else(|_| PathBuf::from(&root));
        let canonical_git_dir =
            std::fs::canonicalize(&git_dir).unwrap_or_else(|_| PathBuf::from(&git_dir));
        let mut hasher = Sha256::new();
        hasher.update(self.inner.device_id.as_bytes());
        hasher.update([0u8]);
        hasher.update(canonical_git_dir.to_string_lossy().as_bytes());
        let id = hex(&hasher.finalize());
        Ok(CheckoutIdentity {
            id,
            root: canonical_root,
            git_dir: canonical_git_dir,
        })
    }

    /// Stable identity shared by the main checkout and every linked worktree
    /// of one repository. Unlike [`Self::checkout_identity`], this hashes the
    /// common git directory, so it follows the repository rather than
    /// whichever branch happened to ask.
    pub async fn repository_identity(&self, path: &Path) -> Result<String, EngineError> {
        let common = self
            .git(
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                Some(path),
            )
            .await?;
        let common = std::fs::canonicalize(&common).unwrap_or_else(|_| PathBuf::from(common));
        let mut hasher = Sha256::new();
        hasher.update(self.inner.device_id.as_bytes());
        hasher.update([0u8]);
        hasher.update(common.to_string_lossy().as_bytes());
        Ok(hex(&hasher.finalize()))
    }

    async fn to_repo(&self, path: &Path) -> Result<Repo, EngineError> {
        let branch = self.current_branch(path).await.ok();
        Ok(Repo {
            path: path.to_string_lossy().to_string(),
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.to_string_lossy().to_string()),
            default_branch: branch,
        })
    }

    // ── ListRepos / AddRepo / CloneRepo / CreateRepo ────────────────────────

    /// Known repos that still exist, each with its current branch. Never fails:
    /// vanished paths and non-repos are silently dropped.
    pub async fn list(&self) -> Vec<Repo> {
        let mut repos = Vec::new();
        for path in self.load_paths() {
            let path = PathBuf::from(path);
            if !Self::path_exists(&path).await || !self.is_repo(&path).await {
                continue;
            }
            match self.to_repo(&path).await {
                Ok(repo) => repos.push(repo),
                Err(err) => {
                    tracing::debug!(path = %path.display(), error = %err, "repo listing skip")
                }
            }
        }
        repos
    }

    /// Remember an existing repository the user pointed us at.
    pub async fn add(&self, path: &str) -> Result<Repo, EngineError> {
        let abs = absolutize(Path::new(path));
        if !Self::path_exists(&abs).await {
            return Err(EngineError::Other(format!(
                "No such folder: {}",
                abs.display()
            )));
        }
        if !self.is_repo(&abs).await {
            return Err(EngineError::Other(format!(
                "Not a git repository: {}",
                abs.display()
            )));
        }
        self.register(&abs.to_string_lossy())?;
        self.to_repo(&abs).await
    }

    /// `git clone` into `{data_dir}/repos/<name>` — the `CloneRepo` verb.
    /// (Named `clone_repo` to keep `Clone::clone` unambiguous on the service
    /// handle.)
    ///
    /// It carries a credential like every other authenticated git operation the
    /// board performs (gh#316). It used to be the one clone path that carried
    /// nothing, which left it authenticating with whatever the *target device's*
    /// ambient git config happened to provide: a `gh` login somebody set up by
    /// hand, or — on a box provisioned an hour ago — nothing at all. A private
    /// repo the board can plainly see then failed on a machine's incidental
    /// configuration, and the operator was told `fatal:` and no more.
    ///
    /// So the caller names the URLs and the credential exactly as
    /// [`Repos::clone_to`] takes them; the only thing this adds is where the
    /// checkout goes.
    pub async fn clone_repo(
        &self,
        url: &str,
        canonical_url: &str,
        env: &[(String, String)],
    ) -> Result<Repo, EngineError> {
        let target = self.clone_root().join(repo_name_from_url(canonical_url));
        self.clone_to(url, canonical_url, &target, env).await
    }

    /// Clone `url` to an exact path, authenticating with `env`, and leave the
    /// checkout's `origin` set to `canonical_url` (gh#97).
    ///
    /// Three things that are true of every clone, and were written down here
    /// first because onboarding is what needed them:
    ///
    /// - **The caller names the path.** Onboarding clones where the operator
    ///   works, not into the data dir, because the route's `repo =` points at
    ///   this directory and a person is going to open it.
    /// - **`env` carries the credential**, so a private repo the board's App can
    ///   see clones without the box's own git identity being involved. The
    ///   secret is never in argv or in the environment — the pairs name an
    ///   askpass helper that mints at the moment git asks.
    /// - **The remote is rewritten afterwards.** The URL that authenticates
    ///   carries `x-access-token@`, which is a username, not a secret — but it
    ///   is a username for a credential this checkout does not own, and a
    ///   checkout outlives the clone that made it. The human who opens this
    ///   folder should find the remote they would have typed.
    ///
    /// The parent directory is created; the target itself must not exist. A
    /// clone that fails leaves nothing behind, so a retry is a clean retry.
    pub async fn clone_to(
        &self,
        url: &str,
        canonical_url: &str,
        target: &Path,
        env: &[(String, String)],
    ) -> Result<Repo, EngineError> {
        if Self::path_exists(target).await {
            return Err(EngineError::Other(format!(
                "Already exists: {}",
                target.display()
            )));
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let target_str = target.to_string_lossy().to_string();
        // Said out loud, on the device doing it (gh#316): the failure that took
        // three watchers and a live reproduction to diagnose was invisible
        // because nothing on either side ever recorded which URL was tried, with
        // what credential, or what git said back.
        tracing::info!(
            url = %loggable_url(url),
            target = %target.display(),
            // Which of the two failures this is going to be, before it is one:
            // a repo the board authenticated for, or one it did not.
            credentialed = env.iter().any(|(k, _)| k == "GIT_ASKPASS"),
            "clone: starting"
        );
        if let Err(err) = self.git_env(&["clone", url, &target_str], None, env).await {
            tracing::warn!(
                url = %loggable_url(url),
                target = %target.display(),
                error = %err,
                "clone failed"
            );
            // A half-written clone is worse than none: it would read as an
            // existing checkout on the next attempt and be *reused*.
            let _ = std::fs::remove_dir_all(target);
            return Err(err);
        }
        tracing::info!(url = %loggable_url(url), target = %target.display(), "clone: done");
        // Best-effort: a checkout that clones but will not answer `remote
        // set-url` is still a working checkout, and failing the onboard over the
        // cosmetics of its remote would be the wrong trade.
        if canonical_url != url
            && let Err(err) = self
                .git(
                    &["remote", "set-url", "origin", canonical_url],
                    Some(target),
                )
                .await
        {
            tracing::warn!(path = %target.display(), error = %err, "clone: origin rewrite failed");
        }
        self.register(&target_str)?;
        self.to_repo(target).await
    }

    /// The `origin` of a checkout, or `None` when the path is not one. The
    /// existing-clone probe onboarding decides reuse on.
    pub async fn origin_url(&self, path: &Path) -> Option<String> {
        self.git_probe(&["remote", "get-url", "origin"], Some(path))
            .await
            .ok()
            .filter(|s| !s.is_empty())
    }

    /// Where [`Repos::clone_repo`] puts a clone — `{data_dir}/repos`, and the
    /// default parent for an onboarding clone that was given no `--dir`.
    pub fn clone_root(&self) -> PathBuf {
        self.inner.data_dir.join("repos")
    }

    /// `git init -b main` a fresh repository under `{data_dir}/repos`.
    pub async fn create(&self, name: &str) -> Result<Repo, EngineError> {
        let clean: String = name
            .trim()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        if clean.is_empty() || clean.chars().all(|c| c == '-' || c == '.') {
            return Err(EngineError::Other("Invalid repository name".into()));
        }
        let target = self.inner.data_dir.join("repos").join(&clean);
        if target.exists() {
            return Err(EngineError::Other(format!(
                "Already exists: {}",
                target.display()
            )));
        }
        std::fs::create_dir_all(&target)?;
        self.git(&["init", "-b", "main"], Some(&target)).await?;
        self.register(&target.to_string_lossy())?;
        self.to_repo(&target).await
    }

    // ── branches ────────────────────────────────────────────────────────────

    /// All branches (`git branch -a`), local first, deduped against their remote
    /// counterparts, with the repo's default branch first.
    pub async fn branches(&self, repo_path: &Path) -> Result<Vec<String>, EngineError> {
        let out = self
            .git(&["branch", "-a", "--format=%(refname)"], Some(repo_path))
            .await?;
        let mut names: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut push = |name: &str| {
            if !name.is_empty() && name != "HEAD" && seen.insert(name.to_string()) {
                names.push(name.to_string());
            }
        };
        // Locals first, then remote-only branches (stripped of their remote prefix).
        for line in out.lines().map(str::trim) {
            if let Some(local) = line.strip_prefix("refs/heads/") {
                push(local);
            }
        }
        for line in out.lines().map(str::trim) {
            if let Some(remote) = line.strip_prefix("refs/remotes/")
                && let Some((_, name)) = remote.split_once('/')
            {
                push(name);
            }
        }
        // Default branch first: origin/HEAD's target, else the checked-out branch.
        let default = match self
            .git(
                &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
                Some(repo_path),
            )
            .await
        {
            Ok(short) => short.split_once('/').map(|(_, b)| b.to_string()),
            Err(_) => None,
        };
        let default = match default {
            Some(branch) => Some(branch),
            None => self
                .current_branch(repo_path)
                .await
                .ok()
                .filter(|b| b != "HEAD"),
        };
        if let Some(default) = default
            && let Some(pos) = names.iter().position(|n| *n == default)
        {
            let head = names.remove(pos);
            names.insert(0, head);
        }
        Ok(names)
    }

    /// [`Self::branches`] enriched with checkout state: which branch the MAIN
    /// folder has checked out (`current`) and which branches are materialized
    /// as linked worktrees (`worktree_path`). Feeds the composer's ref picker
    /// and its checkout-kind selector.
    pub async fn refs(&self, repo_path: &Path) -> Result<Vec<RepoRef>, EngineError> {
        let names = self.branches(repo_path).await?;
        let current = self.current_branch(repo_path).await.ok();
        // `git worktree list --porcelain`: stanzas of `worktree <path>` /
        // `HEAD <sha>` / `branch refs/heads/<name>`. The first stanza is the
        // main checkout — excluded (it's `current`, not a linked worktree).
        let mut worktrees: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        if let Ok(out) = self
            .git(&["worktree", "list", "--porcelain"], Some(repo_path))
            .await
        {
            let mut stanza = 0usize;
            let mut path: Option<String> = None;
            for line in out.lines().map(str::trim) {
                if let Some(p) = line.strip_prefix("worktree ") {
                    stanza += 1;
                    // The first stanza is the main checkout, not a linked tree.
                    path = (stanza > 1).then(|| p.to_string());
                } else if let Some(branch) = line.strip_prefix("branch refs/heads/")
                    && let Some(path) = path.take()
                {
                    worktrees.insert(branch.to_string(), path);
                }
            }
        }
        Ok(names
            .into_iter()
            .map(|name| RepoRef {
                current: current.as_deref() == Some(name.as_str()),
                worktree_path: worktrees.get(&name).cloned(),
                name,
            })
            .collect())
    }

    /// Switch the checkout at `cwd` (a main folder OR a linked worktree) to
    /// `ref_name` — the t3code `switchRef` port: an existing local branch is
    /// checked out directly; a remote-only branch gets a local tracking
    /// branch (`checkout --track origin/<ref>`). A dirty tree or a branch
    /// already checked out in another worktree fails with git's own message.
    /// Returns the resulting current branch.
    pub async fn switch_ref(&self, cwd: &Path, ref_name: &str) -> Result<String, EngineError> {
        let local = self
            .git(
                &[
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{ref_name}"),
                ],
                Some(cwd),
            )
            .await
            .is_ok();
        if local {
            self.git(&["checkout", ref_name], Some(cwd)).await?;
        } else {
            let remote = format!("origin/{ref_name}");
            let has_remote = self
                .git(
                    &[
                        "show-ref",
                        "--verify",
                        "--quiet",
                        &format!("refs/remotes/{remote}"),
                    ],
                    Some(cwd),
                )
                .await
                .is_ok();
            if has_remote {
                self.git(&["checkout", "--track", &remote], Some(cwd))
                    .await?;
            } else {
                // Unknown ref: let git produce the authoritative error.
                self.git(&["checkout", ref_name], Some(cwd)).await?;
            }
        }
        let out = self.git(&["branch", "--show-current"], Some(cwd)).await?;
        Ok(out.trim().to_string())
    }

    // ── worktrees ───────────────────────────────────────────────────────────

    /// `git worktree add` an isolated checkout under
    /// `{worktrees_root}/<repoName>/<generatedName>`, on a fresh `comet/<name>`
    /// branch off `branch`.
    pub async fn create_worktree(
        &self,
        repo_path: &Path,
        branch: &str,
    ) -> Result<Worktree, EngineError> {
        let (path, branch_name, name) = self.plan_worktree(repo_path).await?;
        self.create_planned_worktree(repo_path, branch, path, branch_name, name)
            .await
    }

    /// Reserve the exact names a fork transaction records before invoking
    /// `git worktree add`. Planning does not touch git or create the checkout.
    pub(crate) async fn plan_worktree(
        &self,
        repo_path: &Path,
    ) -> Result<(PathBuf, String, String), EngineError> {
        let repo_name = repo_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".to_string());
        let base = self.inner.worktrees_root.join(&repo_name);
        std::fs::create_dir_all(&base)?;
        // Auto-generate a name colliding with neither an existing dir nor branch.
        let existing: HashSet<String> = self
            .branches(repo_path)
            .await
            .unwrap_or_default()
            .into_iter()
            .collect();
        let mut name = None;
        for attempt in 0..50u64 {
            let seed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64)
                .unwrap_or(attempt)
                .wrapping_add(attempt.wrapping_mul(0x9E37_79B9));
            let candidate = format!(
                "{}-{}",
                ADJECTIVES[(seed % ADJECTIVES.len() as u64) as usize],
                NOUNS[((seed / 31) % NOUNS.len() as u64) as usize]
            );
            let candidate_path = base.join(&candidate);
            let mut planned = self
                .inner
                .planned_worktrees
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !candidate_path.exists()
                && !existing.contains(&format!("comet/{candidate}"))
                && planned.insert(candidate_path)
            {
                name = Some(candidate);
                break;
            }
        }
        let name =
            name.ok_or_else(|| EngineError::Other("Could not allocate a worktree name".into()))?;
        let path = base.join(&name);
        let branch_name = format!("comet/{name}");
        Ok((path, branch_name, name))
    }

    pub(crate) async fn create_planned_worktree(
        &self,
        repo_path: &Path,
        base: &str,
        path: PathBuf,
        branch_name: String,
        name: String,
    ) -> Result<Worktree, EngineError> {
        struct Release<'a>(&'a std::sync::Mutex<HashSet<PathBuf>>, PathBuf);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                self.0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&self.1);
            }
        }
        let _release = Release(&self.inner.planned_worktrees, path.clone());
        if let Err(error) = self
            .git(
                &[
                    "worktree",
                    "add",
                    "-b",
                    &branch_name,
                    &path.to_string_lossy(),
                    base,
                ],
                Some(repo_path),
            )
            .await
        {
            if let Err(cleanup) = self
                .delete_worktree(repo_path, &path, Some(&branch_name))
                .await
            {
                return Err(EngineError::Other(format!(
                    "{error}; rollback after git worktree add failed: {cleanup}"
                )));
            }
            return Err(error);
        }
        let checkout = match self.checkout_identity(&path).await {
            Ok(checkout) => checkout,
            Err(error) => {
                if let Err(cleanup) = self
                    .delete_worktree(repo_path, &path, Some(&branch_name))
                    .await
                {
                    return Err(EngineError::Other(format!(
                        "{error}; rollback of allocated worktree also failed: {cleanup}"
                    )));
                }
                return Err(error);
            }
        };
        Ok(Worktree {
            repo_path: repo_path.to_string_lossy().to_string(),
            path: path.to_string_lossy().to_string(),
            branch: branch_name,
            name,
            checkout_id: Some(checkout.id),
        })
    }

    pub(crate) fn release_planned_worktree(&self, path: &Path) {
        self.inner
            .planned_worktrees
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(path);
    }

    /// Cut a worktree on an exact branch name — the board's dispatch path
    /// (§runtime-impl), where the branch comes from routing.toml's
    /// `branch_template` rather than a generated `comet/…` name.
    ///
    /// A fresh branch starts at `base` **fetched from origin** (gh#67), not at
    /// the space folder's HEAD: on an always-on box that folder sits on
    /// whatever was last checked out there, and branching from it silently
    /// hands every agent a stale main. See [`Self::resolve_base`] for how the
    /// route's `base` key is read, and why a fetch that fails refuses the
    /// dispatch instead of falling back.
    ///
    /// A retry reuses what exists, because git allows a branch in only one
    /// worktree: an existing checkout already on `branch` is returned as-is,
    /// and an existing branch without a checkout is opened rather than re-cut
    /// (it may carry the previous attempt's commits). Neither path fetches or
    /// moves anything — a retry must land on the previous attempt's commits,
    /// never rebase them onto a newer base.
    pub async fn create_worktree_on(
        &self,
        repo_path: &Path,
        branch: &str,
        base: &str,
    ) -> Result<Worktree, EngineError> {
        let repo_name = repo_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".to_string());
        let root = self.inner.worktrees_root.join(&repo_name);
        std::fs::create_dir_all(&root)?;
        let name = branch.replace('/', "-");
        let path = root.join(&name);
        let path_str = path.to_string_lossy().into_owned();

        if path.exists() {
            let current = self.current_branch(&path).await?;
            if current != branch {
                return Err(EngineError::Other(format!(
                    "{} exists but is on {current}, not {branch}",
                    path.display()
                )));
            }
        } else if self.branch_exists(repo_path, branch).await {
            self.prune_worktrees(repo_path).await;
            self.git(&["worktree", "add", &path_str, branch], Some(repo_path))
                .await?;
        } else {
            // Only a fresh branch consults origin — see the doc comment.
            let start = self.resolve_base(repo_path, base).await?;
            self.prune_worktrees(repo_path).await;
            let mut args = vec!["worktree", "add", "-b", branch, &path_str];
            if let Some(start) = start.as_deref() {
                args.push(start);
            }
            self.git(&args, Some(repo_path)).await?;
        }
        let checkout = self.checkout_identity(&path).await?;
        Ok(Worktree {
            repo_path: repo_path.to_string_lossy().to_string(),
            path: path.to_string_lossy().to_string(),
            branch: branch.to_string(),
            name,
            checkout_id: Some(checkout.id),
        })
    }

    /// Fetch the dispatch base from origin and return the ref to cut from —
    /// always a remote-tracking ref, never a local branch. `None` means "cut
    /// from the current HEAD", the explicit opt-out.
    ///
    /// How routing.toml's `base` is read:
    /// - `origin/HEAD` (the default) — the remote's default branch, *asked of
    ///   the remote*: a clone made with `--single-branch`, or one whose
    ///   `refs/remotes/origin/HEAD` was never written, has no local answer, and
    ///   a stale one is the failure this exists to prevent;
    /// - `main` / `origin/main` — that branch on origin;
    /// - `HEAD` (or empty) — the space folder's current HEAD, no network. What
    ///   a repo with no remote needs, said out loud rather than inferred.
    ///
    /// Any other branch on origin works the same way, which is what lets a
    /// dispatch be cut from a *sibling's* branch (gh#285): the board resolves
    /// the parent attempt to its branch name and hands it here, and nothing on
    /// this side needs to know it came from an attempt rather than from
    /// routing.toml. The one thing it does mean is that the parent has to have
    /// pushed — an unpushed branch is not on origin, and the refusal below says
    /// so.
    ///
    /// A fetch that fails (offline, auth, no such branch) is an error and not a
    /// fallback: falling back means an agent branches from a stale main and
    /// nobody finds out until the pull request has the wrong diff in it.
    async fn resolve_base(
        &self,
        repo_path: &Path,
        base: &str,
    ) -> Result<Option<String>, EngineError> {
        let base = base.trim();
        let branch = match base {
            "" | "HEAD" => return Ok(None),
            "origin/HEAD" => self.origin_default_branch(repo_path).await?,
            other => other.strip_prefix("origin/").unwrap_or(other).to_string(),
        };
        // An explicit refspec rather than `git fetch origin <branch>`: the
        // latter leaves updating `refs/remotes/origin/<branch>` to whatever
        // fetch refspec the clone was configured with, and a narrowed one
        // would leave us branching from a remote-tracking ref that never moved.
        self.git(
            &[
                "fetch",
                "--no-tags",
                "origin",
                &format!("+refs/heads/{branch}:refs/remotes/origin/{branch}"),
            ],
            Some(repo_path),
        )
        .await
        .map_err(|e| {
            EngineError::Other(format!(
                "could not fetch `{branch}` from origin in {} ({e}) — refusing to \
                 branch from a possibly stale local checkout. If `{branch}` is \
                 another attempt's branch, that attempt has not pushed it yet: a \
                 dispatch stacks on what is on origin, and there is nothing there \
                 to cut from until the parent pushes",
                repo_path.display()
            ))
        })?;
        Ok(Some(format!("origin/{branch}")))
    }

    /// The remote's default branch, asked of the remote. `set-head --auto` is
    /// the network half (and writes `refs/remotes/origin/HEAD`), `symbolic-ref`
    /// reads what it wrote.
    async fn origin_default_branch(&self, repo_path: &Path) -> Result<String, EngineError> {
        self.git(&["remote", "set-head", "origin", "--auto"], Some(repo_path))
            .await
            .map_err(|e| {
                EngineError::Other(format!(
                    "could not ask origin for its default branch in {} ({e}) — set \
                     `base` on the route to name one explicitly, or `base = \"HEAD\"` \
                     to branch from this checkout",
                    repo_path.display()
                ))
            })?;
        let head = self
            .git(
                &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
                Some(repo_path),
            )
            .await?;
        Ok(head.strip_prefix("origin/").unwrap_or(&head).to_string())
    }

    /// Drop registrations whose worktree directory is gone. Best-effort: a
    /// hand-deleted checkout otherwise fails the next `worktree add` on that
    /// path with "already registered", which fails a dispatch over housekeeping
    /// git will do itself given the chance.
    async fn prune_worktrees(&self, repo_path: &Path) {
        let _ = self.git(&["worktree", "prune"], Some(repo_path)).await;
    }

    async fn branch_exists(&self, path: &Path, branch: &str) -> bool {
        self.git_probe(
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
            Some(path),
        )
        .await
        .is_ok()
    }

    /// Rename a comet-created worktree branch after its chat's generated title
    /// (port of comet's `renameWorktreeBranch`). Guards:
    /// - respect an external checkout/rename: only act while the worktree is still
    ///   on `expected_branch` AND that branch is the original `comet/<folderName>`;
    /// - a title-slug collision gets a stable 6-hex suffix (hash of the worktree
    ///   path); a collision on THAT too fails.
    ///
    /// Returns the branch the worktree ends up on (re-read after the rename so a
    /// concurrent external checkout always wins the metadata race).
    pub async fn rename_worktree_branch(
        &self,
        worktree_path: &Path,
        expected_branch: &str,
        title: &str,
    ) -> Result<String, EngineError> {
        let current = self.current_branch(worktree_path).await?;
        let folder = worktree_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if current != expected_branch || expected_branch != format!("comet/{folder}") {
            return Ok(current);
        }
        let preferred = worktree_branch_from_title(title);
        if preferred == current {
            return Ok(current);
        }
        let mut hasher = Sha256::new();
        hasher.update(worktree_path.to_string_lossy().as_bytes());
        let suffix = &hex(&hasher.finalize())[..6];
        let target = if self.branch_exists(worktree_path, &preferred).await {
            format!("{preferred}-{suffix}")
        } else {
            preferred
        };
        if self.branch_exists(worktree_path, &target).await {
            return Err(EngineError::Other(format!(
                "Branch already exists: {target}"
            )));
        }
        self.git(
            &["branch", "-m", "--", &current, &target],
            Some(worktree_path),
        )
        .await?;
        self.current_branch(worktree_path).await
    }

    /// Best-effort worktree removal (if it still exists), then prune stale refs.
    ///
    /// The branch goes only when it is ours to delete, which is two cases:
    ///
    /// - the worktree is on a `comet/…` branch — one this module generated, so
    ///   nothing else named it;
    /// - `branch` names the branch whose creator vouches for it, and the
    ///   worktree is still on that branch. This is the board's case (gh#72):
    ///   its branches come from `routing.toml`'s `branch_template` and are
    ///   `board/…` by default, so the `comet/` test alone left every dispatched
    ///   branch behind forever.
    ///
    /// Anything else is left alone — the user may have checked out their own
    /// branch inside the worktree, and that one is not ours. A checkout that is
    /// already gone cannot be asked what it was on, so a vouched-for `branch`
    /// is taken at its word there: the caller created it, and git still refuses
    /// to delete a branch checked out in some other worktree.
    pub async fn delete_worktree(
        &self,
        repo_path: &Path,
        worktree_path: &Path,
        branch: Option<&str>,
    ) -> Result<(), EngineError> {
        let current = if worktree_path.exists() {
            self.current_branch(worktree_path).await.unwrap_or_default()
        } else {
            String::new()
        };
        if worktree_path.exists() {
            let removed = self
                .git(
                    &[
                        "worktree",
                        "remove",
                        "--force",
                        &worktree_path.to_string_lossy(),
                    ],
                    Some(repo_path),
                )
                .await;
            if let Err(git_error) = removed {
                // git refused (or the dir is half-gone) — delete the folder directly.
                match std::fs::remove_dir_all(worktree_path) {
                    Ok(()) => {}
                    Err(fs_error) if fs_error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(fs_error) => {
                        return Err(EngineError::Other(format!(
                            "git could not remove {} ({git_error}); filesystem cleanup also failed: {fs_error}",
                            worktree_path.display()
                        )));
                    }
                }
            }
        }
        self.git(&["worktree", "prune"], Some(repo_path)).await?;
        // `current` is empty when the checkout was already gone (or sat on a
        // detached HEAD), which is the case the vouched-for branch exists for.
        let ours = if current.starts_with("comet/") {
            Some(current.as_str())
        } else {
            branch.filter(|b| current.is_empty() || current == *b)
        };
        if let Some(ours) = ours
            && self.branch_exists(repo_path, ours).await
        {
            self.git(&["branch", "-D", ours], Some(repo_path)).await?;
        }
        Ok(())
    }

    // ── ListFolders ─────────────────────────────────────────────────────────

    /// One directory level (home by default): dotfiles hidden, directories first,
    /// capped at [`FOLDER_LIST_MAX_ENTRIES`] with a `truncated` flag. The walk runs
    /// in a spawned blocking task under a 6s wall-clock ceiling — a wedged path
    /// (dead mount, permission-gated folder) fails this listing without blocking
    /// anything else; the abandoned task unwinds on its own thread.
    pub async fn list_folders(&self, path: Option<String>) -> Result<FolderListing, EngineError> {
        self.list_folders_with(path, FOLDER_LIST_TIMEOUT, false)
            .await
    }

    /// `hang_for_test` makes the worker never respond — exercises the timeout path.
    ///
    /// The walk runs on a DETACHED OS thread (not the tokio blocking pool): a
    /// readdir wedged in the kernel can't be cancelled, and a poisoned blocking
    /// pool — or a runtime shutdown waiting on it — must never be possible. On
    /// timeout the thread is simply abandoned (the comet backend's disposable
    /// worker, minus the terminate()).
    #[doc(hidden)]
    pub async fn list_folders_with(
        &self,
        path: Option<String>,
        timeout: Duration,
        hang_for_test: bool,
    ) -> Result<FolderListing, EngineError> {
        let target = match path.filter(|p| !p.trim().is_empty()) {
            Some(p) => absolutize(Path::new(&p)),
            None => home_dir(),
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("folder-list".into())
            .spawn(move || {
                if hang_for_test {
                    // Hold the sender without responding (detached thread; process
                    // exit reclaims it) — the caller must hit its timeout.
                    std::thread::sleep(Duration::from_secs(3600));
                }
                let _ = tx.send(list_folders_blocking(&target));
            });
        if let Err(err) = spawned {
            return Err(EngineError::Other(format!("folder listing failed: {err}")));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(EngineError::Other("folder listing worker exited".into())),
            Err(_) => Err(EngineError::Other(
                "folder listing timed out on the device".into(),
            )),
        }
    }
}

/// The blocking walk: ONE readdir of the target; `is_repo` is a cheap `.git`
/// existence probe per directory entry.
fn list_folders_blocking(target: &Path) -> Result<FolderListing, EngineError> {
    let read = std::fs::read_dir(target).map_err(|e| match e.kind() {
        std::io::ErrorKind::PermissionDenied => {
            EngineError::Other("Comet doesn't have access to this folder on the device.".into())
        }
        _ => EngineError::Other(format!("could not read that folder: {e}")),
    })?;
    let mut entries: Vec<FolderEntry> = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let is_repo = is_dir && entry.path().join(".git").exists();
        entries.push(FolderEntry {
            name,
            is_dir,
            is_repo,
        });
    }
    // Directories first, each group name-sorted (case-insensitive).
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    let truncated = entries.len() > FOLDER_LIST_MAX_ENTRIES;
    entries.truncate(FOLDER_LIST_MAX_ENTRIES);
    Ok(FolderListing {
        path: target.to_string_lossy().to_string(),
        entries,
        truncated,
    })
}

/// Turn a generated chat title into the semantic portion of a Comet branch
/// (port of comet's `worktreeBranchFromTitle`). Comet NFKD-normalizes accented
/// letters first; native keeps it ASCII-only (generated titles are Title Case
/// English), so non-ASCII characters collapse into the `-` separator.
pub fn worktree_branch_from_title(title: &str) -> String {
    let mut slug = String::new();
    for c in title.trim().chars() {
        if matches!(c, '\'' | '"' | '`') {
            continue; // dropped entirely (cafe's → cafes), not a separator
        }
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.truncate(48);
    let slug = slug.trim_matches('-');
    format!("comet/{}", if slug.is_empty() { "update" } else { slug })
}

/// The folder a clone URL should land in — the last path segment, minus `.git`.
/// `git@github.com:o/r.git` and `https://github.com/o/r/` both give `r`.
fn repo_name_from_url(url: &str) -> String {
    url.trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .rsplit(['/', ':'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("repo")
        .to_string()
}

/// A URL safe to write to a log.
///
/// The board's own clone URL carries `x-access-token@`, which is a username and
/// not a secret — but a URL that reaches this from elsewhere may carry
/// `user:password@`, and a log line is exactly the place a password outlives the
/// process that used it. Userinfo with a password in it is replaced whole;
/// a bare username is kept, because it is half of what makes a log line worth
/// having.
fn loggable_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, Some(path)),
        None => (rest, None),
    };
    let redacted = match authority.rsplit_once('@') {
        Some((userinfo, host)) if userinfo.contains(':') => {
            let user = userinfo.split_once(':').map(|(u, _)| u).unwrap_or("");
            format!("{user}:***@{host}")
        }
        _ => authority.to_string(),
    };
    match path {
        Some(path) => format!("{scheme}://{redacted}/{path}"),
        None => format!("{scheme}://{redacted}"),
    }
}

/// Absolute form of a possibly-relative path (no filesystem access).
fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a failed git command puts in the log. The URL stays — gh#316 turned
    /// on which one was used — but a password in it does not.
    #[test]
    fn a_logged_command_keeps_the_url_and_drops_the_password() {
        assert_eq!(
            redacted_command(&["clone", "https://github.com/o/r.git", "/tmp/r"]),
            "git clone https://github.com/o/r.git /tmp/r"
        );
        // The board's own credential form: a username, and worth reading — it
        // is how you tell an authenticated clone from an ambient one.
        assert_eq!(
            redacted_command(&["clone", "https://x-access-token@github.com/o/r.git"]),
            "git clone https://x-access-token@github.com/o/r.git"
        );
        assert_eq!(
            redacted_command(&["clone", "https://user:ghp_secret@github.com/o/r.git"]),
            "git clone https://user:***@github.com/o/r.git"
        );
        // Not userinfo: an `@` in the path, and an SSH remote's `user@host`
        // (no scheme, so nothing to mistake for a password either way).
        assert_eq!(
            loggable_url("https://github.com/o/r/blob/main/a@b"),
            "https://github.com/o/r/blob/main/a@b"
        );
        assert_eq!(
            loggable_url("git@github.com:o/r.git"),
            "git@github.com:o/r.git"
        );
    }

    #[test]
    fn a_clone_url_names_the_folder_it_lands_in() {
        for (url, name) in [
            (
                "https://github.com/bredebjorhovd/itsm-agent.git",
                "itsm-agent",
            ),
            ("https://github.com/bredebjorhovd/itsm-agent/", "itsm-agent"),
            ("git@github.com:bredebjorhovd/itsm-agent.git", "itsm-agent"),
            ("file:///srv/mirrors/thing", "thing"),
            ("", "repo"),
        ] {
            assert_eq!(repo_name_from_url(url), name, "{url}");
        }
    }

    /// A log line is exactly the place a credential outlives the process that
    /// used it (gh#316). The board's own clone URL carries a username and no
    /// password, and that username is worth having in the log; a URL that
    /// arrived from somewhere else may carry both.
    #[test]
    fn a_logged_url_keeps_the_username_and_loses_the_password() {
        assert_eq!(
            loggable_url("https://x-access-token@github.com/o/r.git"),
            "https://x-access-token@github.com/o/r.git"
        );
        assert_eq!(
            loggable_url("https://someone:ghp_secret@github.com/o/r.git"),
            "https://someone:***@github.com/o/r.git"
        );
        assert_eq!(
            loggable_url("https://x-access-token:ghs_secret@github.com"),
            "https://x-access-token:***@github.com"
        );
        // Nothing to redact, and nothing rewritten either.
        assert_eq!(
            loggable_url("git@github.com:o/r.git"),
            "git@github.com:o/r.git"
        );
        assert_eq!(loggable_url("file:///srv/thing"), "file:///srv/thing");
    }
}
