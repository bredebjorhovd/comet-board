//! Board directories, `.env` secrets, and `routing.toml`.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::runtime::{RUNTIME_NAMES, harness_for_runtime};

/// Where the board keeps things: under comet's own data dir, beside the
/// engine's stores. Env overrides exist so tests and a by-hand `doctor` run
/// work against a scratch directory.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Paths> {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        // `~/.comet-native/board` — inside the engine's data dir on purpose:
        // one directory to back up, and its lifetime is already the engine's.
        // `COMET_DATA_DIR` follows the engine's dev-mode override.
        let data_dir = match std::env::var("COMET_DATA_DIR") {
            Ok(v) if !v.is_empty() => PathBuf::from(v),
            _ => PathBuf::from(&home).join(".comet-native"),
        };
        Self::under(&data_dir)
    }

    /// The board directories under an already-resolved engine data dir — the
    /// engine's board service passes its own `EngineConfig::data_dir` here so
    /// config precedence cannot diverge between the two. The
    /// `COMET_BOARD_CONFIG_DIR` / `COMET_BOARD_STATE_DIR` overrides still win,
    /// so tests and a by-hand `doctor` run keep working against scratch dirs.
    pub fn under(data_dir: &Path) -> Result<Paths> {
        let base = data_dir.join("board");
        let config_dir = match std::env::var("COMET_BOARD_CONFIG_DIR") {
            Ok(v) if !v.is_empty() => PathBuf::from(v),
            _ => base.clone(),
        };
        let state_dir = match std::env::var("COMET_BOARD_STATE_DIR") {
            Ok(v) if !v.is_empty() => PathBuf::from(v),
            _ => base.join("state"),
        };
        std::fs::create_dir_all(&config_dir)
            .with_context(|| format!("creating config dir {}", config_dir.display()))?;
        std::fs::create_dir_all(&state_dir)
            .with_context(|| format!("creating state dir {}", state_dir.display()))?;
        Ok(Paths {
            config_dir,
            state_dir,
        })
    }

    pub fn db(&self) -> PathBuf {
        self.state_dir.join("board.db")
    }
    pub fn routing(&self) -> PathBuf {
        self.config_dir.join("routing.toml")
    }
    pub fn env_file(&self) -> PathBuf {
        self.config_dir.join(".env")
    }
    pub fn pidfile(&self) -> PathBuf {
        self.state_dir.join("syncd.pid")
    }
    pub fn logfile(&self) -> PathBuf {
        self.state_dir.join("syncd.log")
    }
}

/// Where attempt checkouts live — the engine's worktree root, named here
/// because two crates need the same answer and only one may own it.
///
/// The engine cuts them (`crates/engine/src/repos.rs`) and the board reclaims
/// and reports on them (gh#72), so a second copy of this rule would mean
/// `doctor` measuring a directory nothing writes to. Deliberately NOT under the
/// data dir — worktrees are user-facing working checkouts.
/// `COMET_WORKTREES_DIR` overrides (test isolation); empty reads as unset.
pub fn worktrees_root() -> PathBuf {
    std::env::var_os("COMET_WORKTREES_DIR")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".comet-native").join("worktrees")
        })
}

/// Credentials effective for one configuration read.
///
/// Shell variables take precedence over `.env`, but file values are read
/// directly instead of copied into the process environment. That distinction
/// lets long-lived board and daemon processes observe both added and edited
/// keys on their next reload.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Credentials {
    pub(crate) linear_api_key: Option<String>,
    pub(crate) github_token: Option<String>,
    /// The GitHub App's numeric id, and the PEM of its private key on disk
    /// (gh#58). Both or neither: one alone is a half-configured App, which
    /// leaves the board silently on whatever `GITHUB_TOKEN` says.
    pub(crate) github_app_id: Option<String>,
    pub(crate) github_app_key_path: Option<String>,
}

/// Which GitHub credential is in force.
///
/// The App wins when it is configured, and `GITHUB_TOKEN` keeps working when it
/// is not — a single-owner self-host should not be pushed onto registering an
/// App, and every board already running has to survive this change untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GithubAuth {
    /// Nothing configured. Public repos, anonymously.
    None,
    /// A personal access token.
    Token(String),
    /// A GitHub App, minting one token per installation.
    App { app_id: String, key_path: PathBuf },
}

impl Credentials {
    pub fn load(paths: &Paths) -> Credentials {
        Self::load_with(paths, |key| std::env::var(key))
    }

    fn load_with<F>(paths: &Paths, inherited: F) -> Credentials
    where
        F: Fn(&str) -> std::result::Result<String, std::env::VarError>,
    {
        Credentials {
            linear_api_key: credential(paths, "LINEAR_API_KEY", &inherited),
            github_token: credential(paths, "GITHUB_TOKEN", &inherited),
            github_app_id: credential(paths, "GITHUB_APP_ID", &inherited),
            github_app_key_path: credential(paths, "GITHUB_APP_PRIVATE_KEY_PATH", &inherited),
        }
    }

    /// The GitHub credential to authenticate with, App first.
    pub fn github_auth(&self) -> GithubAuth {
        match (&self.github_app_id, &self.github_app_key_path) {
            (Some(app_id), Some(key_path)) => GithubAuth::App {
                app_id: app_id.clone(),
                key_path: PathBuf::from(key_path),
            },
            _ => match &self.github_token {
                Some(t) => GithubAuth::Token(t.clone()),
                None => GithubAuth::None,
            },
        }
    }

    /// Set one App key and not the other and the board quietly stays on the
    /// personal access token — no error, no log line, just writes attributed to
    /// a person instead of a `[bot]`. Named here so `doctor` can say it out loud.
    pub fn github_app_half_configured(&self) -> Option<&'static str> {
        match (&self.github_app_id, &self.github_app_key_path) {
            (Some(_), None) => Some("GITHUB_APP_PRIVATE_KEY_PATH"),
            (None, Some(_)) => Some("GITHUB_APP_ID"),
            _ => None,
        }
    }
}

fn credential<F>(paths: &Paths, key: &str, inherited: &F) -> Option<String>
where
    F: Fn(&str) -> std::result::Result<String, std::env::VarError>,
{
    match inherited(key) {
        // An explicitly empty shell variable still overrides the file.
        Ok(value) => return (!value.is_empty()).then_some(value),
        Err(std::env::VarError::NotUnicode(_)) => return None,
        Err(std::env::VarError::NotPresent) => {}
    }

    dotenvy::from_path_iter(paths.env_file())
        .ok()?
        .filter_map(std::result::Result::ok)
        .find_map(|(name, value)| (name == key).then_some(value))
        .filter(|value| !value.is_empty())
}

pub fn linear_api_key(paths: &Paths) -> Option<String> {
    Credentials::load(paths).linear_api_key
}

pub fn github_token(paths: &Paths) -> Option<String> {
    Credentials::load(paths).github_token
}

/// The GitHub credential mode in force, read off `.env`.
pub fn github_auth(paths: &Paths) -> GithubAuth {
    Credentials::load(paths).github_auth()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoutingConfig {
    #[serde(default)]
    pub sync: SyncConfig,
    #[serde(default, rename = "route")]
    pub routes: Vec<Route>,
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub github: GithubConfig,
    #[serde(default)]
    pub linear: LinearConfig,
    #[serde(default)]
    pub adopt: AdoptConfig,
}

/// What the board offers to adopt, and what it has been told to stop offering.
///
/// Only an exclusion list: adoption itself writes ordinary `[[route]]` and
/// `[github] repos` entries, because those are the config that already exists.
/// Ignoring has nowhere else to live — "I am only reading this repo" is not a
/// fact any other key can carry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AdoptConfig {
    /// `owner/repo` entries the board will never offer again. Delete a line to
    /// be offered it once more.
    #[serde(default)]
    pub ignore: Vec<String>,
}

impl AdoptConfig {
    pub fn ignores(&self, slug: &str) -> bool {
        self.ignore.iter().any(|i| i.eq_ignore_ascii_case(slug))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConfig {
    /// Poll interval, e.g. `"30s"`.
    #[serde(default = "default_interval")]
    pub interval: String,
    /// Linear labels that mean "dispatchable".
    #[serde(default)]
    pub labels: Vec<String>,
}

fn default_interval() -> String {
    "30s".into()
}

impl Default for SyncConfig {
    fn default() -> Self {
        SyncConfig {
            interval: default_interval(),
            labels: Vec::new(),
        }
    }
}

impl SyncConfig {
    /// Parse `30s` / `5m` / `90` (bare = seconds). Clamped to a sane floor so a
    /// typo cannot hammer Linear.
    pub fn interval_secs(&self) -> u64 {
        parse_duration_secs(&self.interval).unwrap_or(30).max(5)
    }
}

pub fn parse_duration_secs(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, mult) = match s.chars().last()? {
        's' => (&s[..s.len() - 1], 1),
        'm' => (&s[..s.len() - 1], 60),
        'h' => (&s[..s.len() - 1], 3600),
        // `d` and `w` are for `gc --older-than`, where the interesting units are
        // days and weeks rather than the sync interval's seconds.
        'd' => (&s[..s.len() - 1], 86_400),
        'w' => (&s[..s.len() - 1], 604_800),
        _ => (s, 1),
    };
    num.trim().parse::<u64>().ok().map(|n| n * mult)
}

/// Parse a `max_duration` value into a cap in seconds (gh#70).
///
/// `Ok(None)` is *no cap*, said out loud — `off`, `none`, `never` or `0`. An
/// unparseable value is an `Err` rather than a silent `None`, because the two
/// look identical on the board and only one of them is what somebody meant:
/// `RoutingConfig::validate` refuses the config instead of leaving a typo
/// reading as "unlimited". Anything shorter than [`crate::overrun::MIN_CAP_SECS`]
/// is raised to it — a cap that expires before the first turn finishes would
/// fail every dispatch on the route.
pub fn parse_max_duration(s: &str) -> std::result::Result<Option<u64>, String> {
    let t = s.trim();
    if matches!(t.to_ascii_lowercase().as_str(), "off" | "none" | "never") {
        return Ok(None);
    }
    match parse_duration_secs(t) {
        Some(0) => Ok(None),
        Some(n) => Ok(Some(n.max(crate::overrun::MIN_CAP_SECS))),
        None => Err(format!(
            "`{s}` is not a duration; write it like `2h`, `90m`, `3600`, or `off`"
        )),
    }
}

/// Parse a `retain_worktrees` value into a retention window in seconds (gh#72).
///
/// `Ok(None)` is *keep forever*, said out loud — `off`, `none`, `never` or `0`
/// — and it is the only spelling that turns worktree collection off. An
/// unparseable value is an `Err` for the same reason `max_duration`'s is: a
/// typo that read as "never" would leave the disk filling up silently, which is
/// the bug this whole feature is about. No minimum: a retention of seconds is
/// what the tests want and what an operator reclaiming a full disk means.
pub fn parse_retention(s: &str) -> std::result::Result<Option<u64>, String> {
    let t = s.trim();
    if matches!(t.to_ascii_lowercase().as_str(), "off" | "none" | "never") {
        return Ok(None);
    }
    match parse_duration_secs(t) {
        Some(0) => Ok(None),
        Some(n) => Ok(Some(n)),
        None => Err(format!(
            "`{s}` is not a duration; write it like `7d`, `2w`, `48h`, or `off`"
        )),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Defaults {
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_per_workspace: usize,
    #[serde(default = "default_branch_template")]
    pub branch_template: String,
    /// The ref every dispatch's branch is cut from, fetched from origin first
    /// (gh#67).
    ///
    /// `origin/HEAD` (the default) is the remote's default branch, asked of the
    /// remote at dispatch time. `main` or `origin/main` names a branch on
    /// origin. `HEAD` is the opt-out: branch from the space folder's current
    /// HEAD with no network at all — right for a repo with no remote, wrong for
    /// an always-on box, which is why it is not the default. Whatever it says,
    /// the dispatch never branches from a local branch: the folder sits on
    /// whatever was last checked out in it, and that is nobody's intended base.
    #[serde(default = "default_base")]
    pub base: String,
    /// Surface a notification, out of band, when work blocks or settles.
    ///
    /// A conversational orchestrator cannot be woken — it only gets a turn when
    /// something prompts it — so the operator is the one who has to notice, and
    /// an agent that stops to ask at 02:00 is invisible until somebody looks at
    /// the board. This is the switch for the channel that reaches them anyway;
    /// [`notify_webhook`](Self::notify_webhook) is where it goes. On with
    /// nothing configured is a channel with no address, which `doctor` says
    /// plainly rather than reporting a notice that cannot fire.
    ///
    /// It does not gate the upstream comments — a blocked attempt comments on
    /// its own issue regardless, because that trail belongs to the task and not
    /// to whoever is watching tonight.
    #[serde(default = "default_true")]
    pub notify: bool,
    /// Where [`notify`](Self::notify) sends: one URL, POSTed a small JSON body
    /// (`{"event": "on_blocked" | "on_settled", …}`) when a dispatched attempt
    /// blocks or settles.
    ///
    /// One URL and no per-service integration on purpose. Slack, email and
    /// pagers all already accept a webhook — via their own incoming-webhook
    /// endpoint, or the two lines of glue an operator already has — and the
    /// board carrying a client per destination would be the board maintaining
    /// three credentials it never reads.
    #[serde(default)]
    pub notify_webhook: Option<String>,
    /// Also tell the *agent* that released a task when its work settles, by
    /// queueing a message into the chat it dispatched from (AGE-25).
    ///
    /// Off by default, and that is the whole design constraint rather than
    /// caution: an orchestrator woken by every child it released is one that
    /// cannot hold a train of thought. Turn it on when the chats you dispatch
    /// from are orchestrators that want to act on outcomes, not when they are
    /// you at a prompt — [`notify`](Self::notify) already covers that.
    ///
    /// Independent of `notify`, because they are different audiences: this one
    /// never fires for operator-released work, which has no dispatcher.
    #[serde(default)]
    pub notify_dispatcher: bool,
    /// Which tracker `comet-board new` writes to: `linear` or `github`.
    ///
    /// Not inferable from a label — a label routes work to a repo and says
    /// nothing about where that project's tickets live. Set the habit here and
    /// override per ticket with `--source`.
    #[serde(default = "default_new_source")]
    pub new_source: String,
    /// How long a single attempt may stay live before the board warns its chat
    /// and then closes it `failed` (gh#70). Overridden per route.
    ///
    /// Two hours by default — long enough for the work a dispatch is worth
    /// giving an agent, short enough that a looping one is noticed the same
    /// afternoon. `off` (or `0`) removes the cap entirely, which is the honest
    /// spelling of what every board did before this existed.
    #[serde(default = "default_max_duration")]
    pub max_duration: String,
    /// How long a finished attempt's checkout is kept before the board deletes
    /// it and its local branch (gh#72). The clock starts when the attempt is
    /// closed *and* its task has left the board — merged, closed upstream, or
    /// marked done — never while an attempt is live or a pull request is open.
    ///
    /// A week by default: long enough to open last Tuesday's checkout and see
    /// what the agent actually did, short enough that a box dispatching a few
    /// tasks a day does not fill its disk with them. `off` (or `0`) keeps every
    /// checkout forever, which is what every board did before this existed —
    /// and what `doctor`'s worktree check exists to make visible.
    #[serde(default = "default_retain_worktrees")]
    pub retain_worktrees: String,
}

fn default_max_duration() -> String {
    "2h".into()
}

fn default_retain_worktrees() -> String {
    "7d".into()
}

fn default_new_source() -> String {
    "linear".into()
}

fn default_max_concurrent() -> usize {
    3
}

/// Impl spec §5. The design fixtures show `lin-145-altinn-retry`, but the design
/// handoff (#11) defers to the impl spec here, and it is config either way.
fn default_branch_template() -> String {
    "board/{identifier_lower}".into()
}

/// The remote's default branch — the base a person means when they say "cut it
/// from main" without naming which main.
fn default_base() -> String {
    "origin/HEAD".into()
}

impl Default for Defaults {
    fn default() -> Self {
        Defaults {
            max_concurrent_per_workspace: default_max_concurrent(),
            branch_template: default_branch_template(),
            base: default_base(),
            notify: true,
            notify_webhook: None,
            notify_dispatcher: false,
            new_source: default_new_source(),
            max_duration: default_max_duration(),
            retain_worktrees: default_retain_worktrees(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubConfig {
    /// `owner/repo` entries to poll for issues and PRs.
    #[serde(default)]
    pub repos: Vec<String>,
    /// Only surface issues carrying one of these labels. Empty = all.
    ///
    /// The *fallback*, not the law: a repo with a `[[github.repo]]` table of its
    /// own answers from that instead. See [`GithubConfig::labels_for`].
    #[serde(default)]
    pub labels: Vec<String>,
    /// Per-repo overrides, keyed by the same `owner/repo` that `repos` lists.
    ///
    /// One global `labels` asks every repo the same question, and repos want
    /// different answers: a curated backlog means `labels = []` correctly says
    /// "everything", while a repo carrying its whole roadmap as open issues
    /// needs "only what is current" — and its issues already say which those
    /// are. Without this the board had no way to be told.
    #[serde(default, rename = "repo")]
    pub per_repo: Vec<RepoConfig>,
    /// Show open pull requests as their own `review` rows.
    ///
    /// A PR raised by a board dispatch attaches to its task instead; this is
    /// about the ones nobody dispatched, which are still work waiting on you.
    #[serde(default = "default_true")]
    pub pull_requests: bool,
    /// Leave the same trail on GitHub that Linear gets: a comment on dispatch
    /// and on outcome, and close the issue when the task is done.
    ///
    /// **Off by default.** Writing to someone's issues is not a thing to start
    /// doing because they pointed the board at a repo — the first dispatch
    /// would comment on production issues before anyone had decided that was
    /// wanted. `d mark done` stays honest without it: the local override moves
    /// the row and survives re-derivation, it just does not close the issue
    /// upstream.
    ///
    /// The *fallback*, not the law, for the same reason `labels` is: a board
    /// spanning a personal project and a production repo wants the trail on one
    /// and not the other, and a single flag can only be set by its riskiest
    /// repo. See [`GithubConfig::writeback_for`].
    #[serde(default)]
    pub writeback: bool,
    /// Wake the agent that wrote a pull request when somebody reviews it.
    ///
    /// The agent is still sitting in its pane with the whole task in context —
    /// a task in `review` keeps its pane, so the author is alive and idle — and
    /// a review is a notification it has no way to receive. This delivers new
    /// comments into that pane.
    ///
    /// **On by default**, unlike [`GithubConfig::writeback`]: this writes
    /// nothing to anybody's repository. It types into a pane of your own.
    #[serde(default = "default_true")]
    pub deliver_reviews: bool,
}

fn default_true() -> bool {
    true
}

impl Default for GithubConfig {
    fn default() -> Self {
        GithubConfig {
            repos: Vec::new(),
            labels: Vec::new(),
            per_repo: Vec::new(),
            pull_requests: true,
            writeback: false,
            deliver_reviews: true,
        }
    }
}

/// Settings for one repo, overriding the `[github]` defaults.
///
/// ```toml
/// [[github.repo]]
/// name = "bredebjorhovd/itsm-agent"
/// labels = ["release-a"]
/// writeback = false
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoConfig {
    /// `owner/repo`. Must also appear in `[github] repos` — see
    /// [`RoutingConfig::validate`].
    pub name: String,
    /// Only surface issues carrying one of these labels.
    ///
    /// Absent — not empty — is what falls back to `[github] labels`. The
    /// difference is the whole point: `labels = []` on a repo is an operator
    /// saying "everything, and I mean it here" over a global filter, which no
    /// single global list can express.
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    /// Whether the board writes to this repo's issues, overriding
    /// `[github] writeback`.
    ///
    /// Absent falls back to the global flag. Set it where the answer differs:
    /// comments on a repo of your own are provenance nobody minds, and the same
    /// comments on a production repo land on issues other people read. One
    /// global flag makes that choice once for every repo, so it gets set by the
    /// riskiest one — off, and the board can close nothing anywhere.
    #[serde(default)]
    pub writeback: Option<bool>,
}

impl GithubConfig {
    /// The settings written for one repo, if any.
    pub fn settings_for(&self, repo: &str) -> Option<&RepoConfig> {
        self.per_repo
            .iter()
            .find(|r| r.name.eq_ignore_ascii_case(repo))
    }

    /// Which labels to poll one repo for, falling back to the global list.
    pub fn labels_for(&self, repo: &str) -> &[String] {
        self.settings_for(repo)
            .and_then(|r| r.labels.as_deref())
            .unwrap_or(&self.labels)
    }

    /// Whether the board writes to one repo's issues, falling back to the
    /// global flag.
    ///
    /// A repo that is not in `repos` at all answers from the global flag too.
    /// That is the safe reading for the only caller that can ask about one — a
    /// writeback queued for a repo since removed from the config — because the
    /// flag it falls back to is the one the operator set for repos in general.
    pub fn writeback_for(&self, repo: &str) -> bool {
        self.settings_for(repo)
            .and_then(|r| r.writeback)
            .unwrap_or(self.writeback)
    }

    /// The configured repos the board will write to, in `repos` order.
    ///
    /// `doctor` reports these by name. Once the answer differs per repo, a
    /// global `ON` says nothing about the repo the operator is actually worried
    /// about — the point is to see, without reading config, which list it is in.
    pub fn writeback_repos(&self) -> Vec<&str> {
        self.repos
            .iter()
            .map(String::as_str)
            .filter(|r| self.writeback_for(r))
            .collect()
    }

    /// The configured repos the board only reads. The complement of
    /// [`GithubConfig::writeback_repos`].
    pub fn read_only_repos(&self) -> Vec<&str> {
        self.repos
            .iter()
            .map(String::as_str)
            .filter(|r| !self.writeback_for(r))
            .collect()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LinearConfig {
    /// Name of the workflow state a task moves to when the board derives
    /// `review` — typically `"In Review"`.
    ///
    /// Unset by default, and unset means no transition at all: the ticket stays
    /// wherever dispatch left it, which is what the board did before this
    /// existed.
    ///
    /// A *name*, unavoidably. Every other Linear state the board touches is
    /// resolved by type, because teams rename these freely — but Linear has no
    /// review type. `In Review` and `In Progress` are both `type: started`, so
    /// the API cannot be asked which one means review, and the lowest-position
    /// `started` state (what dispatch uses) is `In Progress` precisely because
    /// review comes after it. Guessing the name would break a renamed or
    /// non-English workflow silently; naming it here is the only mapping that
    /// is explicit. `doctor` checks it resolves.
    #[serde(default)]
    pub review_state: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    /// Name shown in the picker and the prompt view. Defaults to the workspace.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, rename = "match")]
    pub match_: RouteMatch,
    /// Name of the comet *space* (device + folder pair) work dispatches into.
    /// Kept as `workspace` in the config format: it is the same concept herdr
    /// called by that name, and renaming the key would break every ported
    /// routing.toml for zero information.
    pub workspace: String,
    pub repo: String,
    pub runtime: String,
    /// Agent-account slot id this route's dispatches run under — whose Claude
    /// or Codex subscription pays for them (gh#59). Omitted means the device's
    /// own CLI login, which is the whole story on a single-user box. A
    /// dispatch's `--account` overrides it; `comet-board doctor` lists the ids
    /// this device has saved.
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    /// Per-route override of `defaults.branch_template`.
    #[serde(default)]
    pub branch_template: Option<String>,
    /// Per-route override of `defaults.base` — the ref a dispatch's branch is
    /// cut from. See [`Defaults::base`].
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub max_concurrent: Option<usize>,
    /// Per-route override of `defaults.max_duration` (gh#70) — the wall-clock
    /// cap on one attempt, e.g. `"6h"`, or `"off"` for none.
    ///
    /// Per route because the answer is a property of the work: a route pointed
    /// at a refactor across a monorepo is not the route that fixes typos, and a
    /// single number has to be set by whichever of them runs longest — which
    /// leaves the looping agent on every other route running until somebody
    /// looks.
    #[serde(default)]
    pub max_duration: Option<String>,
}

impl Route {
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.workspace)
    }

    /// Expand `~` in the configured repo path.
    pub fn repo_path(&self) -> PathBuf {
        expand_tilde(&self.repo)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RouteMatch {
    pub linear_team: Option<String>,
    pub linear_project: Option<String>,
    pub gh_repo: Option<String>,
    pub label: Option<String>,
}

impl RouteMatch {
    /// A route with no `match` matches everything, so it must come last —
    /// [`RoutingConfig::validate`] refuses a config where one does not, and the
    /// adoption writer inserts ahead of it rather than after.
    pub fn is_empty(&self) -> bool {
        self.linear_team.is_none()
            && self.linear_project.is_none()
            && self.gh_repo.is_none()
            && self.label.is_none()
    }
}

/// The reverse of [`expand_tilde`], for display: a home-relative path fits on a
/// terminal row where an absolute one gets truncated exactly where the useful
/// part is.
pub fn shorten_home(p: &Path) -> String {
    let s = p.display().to_string();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && s.starts_with(&home) => {
            format!("~{}", &s[home.len()..])
        }
        _ => s,
    }
}

pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(p)
}

/// The facts about a task a route can match on.
#[derive(Debug, Clone, Default)]
pub struct RouteContext {
    pub linear_team: Option<String>,
    pub linear_project: Option<String>,
    pub gh_repo: Option<String>,
    pub labels: Vec<String>,
}

impl RoutingConfig {
    pub fn load(path: &Path) -> Result<RoutingConfig> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: RoutingConfig =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Parse without validating. `doctor` uses this so that one bad route does
    /// not hide the problems in every other route.
    pub fn load_unvalidated(path: &Path) -> Result<RoutingConfig> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn check(&self) -> Result<()> {
        self.validate()
    }

    /// Load if present; an absent file is not an error (the board renders with
    /// every row marked `no route`, which is the honest empty state).
    pub fn load_or_default(path: &Path) -> RoutingConfig {
        RoutingConfig::load(path).unwrap_or_default()
    }

    fn validate(&self) -> Result<()> {
        // Refusing on the first problem is the load path's contract: a config
        // that is wrong anywhere is not used at all, and the first reason is
        // the one to act on. The rest are still worth *seeing*, which is what
        // [`RoutingConfig::problems`] is for.
        match self.problems().into_iter().next() {
            Some(problem) => bail!(problem),
            None => Ok(()),
        }
    }

    /// Everything wrong with this config, in file order.
    ///
    /// The same checks [`RoutingConfig::validate`] refuses on, collected rather
    /// than stopped at the first — an editor showing one problem at a time
    /// turns fixing three of them into three round trips, and the reader of a
    /// remote box's config cannot see the file to spot the rest.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (i, r) in self.routes.iter().enumerate() {
            if r.match_.is_empty() {
                // A catch-all route is legal and useful, but it must be last or
                // it silently shadows everything after it.
                if i + 1 != self.routes.len() {
                    out.push(format!(
                        "route {} ({}) has an empty `match` but is not last; \
                         first matching route wins, so it would shadow the {} route(s) after it",
                        i + 1,
                        r.display_name(),
                        self.routes.len() - i - 1
                    ));
                }
            }
            if harness_for_runtime(&r.runtime).is_none() {
                out.push(format!(
                    "route {} ({}) has runtime `{}`, which is not a comet harness. \
                     Known runtimes: {}",
                    i + 1,
                    r.display_name(),
                    r.runtime,
                    RUNTIME_NAMES.join(", ")
                ));
            }
            // A typo here reads exactly like "no cap" on the board, and only
            // one of those is what anybody meant (gh#70).
            if let Some(d) = &r.max_duration
                && let Err(e) = parse_max_duration(d)
            {
                out.push(format!(
                    "route {} ({}) has max_duration {e}",
                    i + 1,
                    r.display_name()
                ));
            }
        }
        if let Err(e) = parse_max_duration(&self.defaults.max_duration) {
            out.push(format!("[defaults] max_duration {e}"));
        }
        // Same reasoning as the cap above: an unparseable retention would read
        // as `off` on a board nobody told, and the checkouts would pile up
        // exactly as they did before gh#72.
        if let Err(e) = parse_retention(&self.defaults.retain_worktrees) {
            out.push(format!("[defaults] retain_worktrees {e}"));
        }
        // `[github] repos` stays the one list of what is polled, so a
        // `[[github.repo]]` naming anything else is settings that apply to
        // nothing — silently, which is the failure this table exists to fix.
        for (i, r) in self.github.per_repo.iter().enumerate() {
            if !self
                .github
                .repos
                .iter()
                .any(|listed| listed.eq_ignore_ascii_case(&r.name))
            {
                out.push(format!(
                    "[[github.repo]] name = \"{}\" is not in `[github] repos`, so nothing \
                     would ever use it. Add it to `repos`, or correct the name.",
                    r.name
                ));
            }
            if self.github.per_repo[..i]
                .iter()
                .any(|earlier| earlier.name.eq_ignore_ascii_case(&r.name))
            {
                out.push(format!(
                    "[[github.repo]] name = \"{}\" appears twice; only the first would \
                     be used, so the second is settings that do nothing",
                    r.name
                ));
            }
        }
        out
    }

    /// The Linear teams this config actually dispatches for, deduplicated.
    ///
    /// Empty is the answer that matters: a config no route of which names a
    /// team is a board that wants nothing from Linear, and `doctor` reads that
    /// as "GitHub only" rather than as a missing credential (gh#96).
    pub fn linear_teams(&self) -> Vec<&str> {
        let mut teams: Vec<&str> = self
            .routes
            .iter()
            .filter_map(|r| r.match_.linear_team.as_deref())
            .collect();
        teams.sort_unstable();
        teams.dedup();
        teams
    }

    /// First matching route wins (impl spec §5).
    pub fn resolve(&self, ctx: &RouteContext) -> Option<&Route> {
        self.routes.iter().find(|r| route_matches(&r.match_, ctx))
    }

    pub fn branch_template<'a>(&'a self, route: &'a Route) -> &'a str {
        route
            .branch_template
            .as_deref()
            .unwrap_or(&self.defaults.branch_template)
    }

    /// The ref this route's dispatches branch from — the route's `base`, else
    /// `defaults.base`, else `origin/HEAD`.
    pub fn base<'a>(&'a self, route: &'a Route) -> &'a str {
        route.base.as_deref().unwrap_or(&self.defaults.base)
    }

    pub fn max_concurrent(&self, route: &Route) -> usize {
        route
            .max_concurrent
            .unwrap_or(self.defaults.max_concurrent_per_workspace)
    }

    /// The wall-clock cap for attempts on a route, in seconds — the route's
    /// own `max_duration`, else `defaults.max_duration`. `None` is uncapped
    /// (gh#70).
    ///
    /// `route` is an `Option` because the caller is reconciliation, not
    /// dispatch: an attempt outlives the config that released it, and a route
    /// renamed or deleted under a running agent must still leave that agent
    /// bounded. No route means the default cap, which is the safe reading —
    /// falling back to "unlimited" would make deleting a route the way to
    /// escape the cap.
    pub fn max_duration_secs(&self, route: Option<&Route>) -> Option<u64> {
        let raw = route
            .and_then(|r| r.max_duration.as_deref())
            .unwrap_or(&self.defaults.max_duration);
        // Validation has already refused an unparseable value; a config that
        // reached here with one is one `load_or_default` fell back on, so the
        // default cap is the honest answer rather than none at all.
        parse_max_duration(raw)
            .unwrap_or_else(|_| parse_max_duration(&default_max_duration()).ok().flatten())
    }

    /// How long a finished attempt's checkout is kept, in seconds. `None` is
    /// "forever" — collection off (gh#72).
    ///
    /// Board-wide rather than per route: which repo an attempt ran in says
    /// nothing about how long you want to be able to look at its checkout, and
    /// a per-route window would be one more place for the disk to fill up
    /// behind a key nobody set. An unparseable value falls back to the default
    /// window, never to "forever", for the same reason the cap does.
    pub fn retain_worktrees_secs(&self) -> Option<u64> {
        parse_retention(&self.defaults.retain_worktrees)
            .unwrap_or_else(|_| parse_retention(&default_retain_worktrees()).ok().flatten())
    }
}

/// All *specified* keys must match (AND). Unspecified keys are ignored.
fn route_matches(m: &RouteMatch, ctx: &RouteContext) -> bool {
    if let Some(team) = &m.linear_team
        && ctx.linear_team.as_deref() != Some(team.as_str())
    {
        return false;
    }
    if let Some(project) = &m.linear_project
        && ctx.linear_project.as_deref() != Some(project.as_str())
    {
        return false;
    }
    if let Some(repo) = &m.gh_repo
        && !ctx
            .gh_repo
            .as_deref()
            .is_some_and(|r| r.eq_ignore_ascii_case(repo))
    {
        return false;
    }
    if let Some(label) = &m.label
        && !ctx.labels.iter().any(|l| l.eq_ignore_ascii_case(label))
    {
        return false;
    }
    true
}

/// Interpolate `{key}` placeholders. Unknown placeholders are left untouched
/// rather than blanked, so a typo is visible in the prompt view instead of
/// silently sending an empty string to the agent.
pub fn interpolate(template: &str, vars: &BTreeMap<&str, String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) => {
                let key = &after[..end];
                match vars.get(key) {
                    Some(v) => out.push_str(v),
                    None => {
                        out.push('{');
                        out.push_str(key);
                        out.push('}');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push('{');
                rest = after;
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Slugify for branch names: lowercase, non-alphanumerics collapsed to `-`.
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[sync]
interval = "30s"
labels = ["herd"]

[[route]]
match = { linear_team = "OFF" }
workspace = "offhand"
repo = "~/code/offhand"
runtime = "claude-code"
prompt = """
You are working on: {title} ({identifier})
{body}
"""

[[route]]
match = { label = "fintech" }
workspace = "fintech"
repo = "~/code/tripletex-int"
runtime = "claude-code"

[defaults]
max_concurrent_per_workspace = 3
branch_template = "board/{identifier_lower}"
"#;

    fn cfg() -> RoutingConfig {
        let c: RoutingConfig = toml::from_str(SAMPLE).unwrap();
        c.validate().unwrap();
        c
    }

    #[test]
    fn parses_the_spec_example() {
        let c = cfg();
        assert_eq!(c.routes.len(), 2);
        assert_eq!(c.sync.interval_secs(), 30);
        assert_eq!(c.sync.labels, vec!["herd"]);
        assert_eq!(c.defaults.max_concurrent_per_workspace, 3);
    }

    #[test]
    fn first_matching_route_wins() {
        let c = cfg();
        // Matches both the team route and the label route; the team route is
        // declared first.
        let ctx = RouteContext {
            linear_team: Some("OFF".into()),
            labels: vec!["fintech".into()],
            ..Default::default()
        };
        assert_eq!(c.resolve(&ctx).unwrap().workspace, "offhand");
    }

    #[test]
    fn label_route_matches_when_team_does_not() {
        let c = cfg();
        let ctx = RouteContext {
            linear_team: Some("TAL".into()),
            labels: vec!["fintech".into()],
            ..Default::default()
        };
        assert_eq!(c.resolve(&ctx).unwrap().workspace, "fintech");
    }

    #[test]
    fn unmatched_task_has_no_route() {
        let c = cfg();
        let ctx = RouteContext {
            linear_team: Some("TAL".into()),
            labels: vec!["chore".into()],
            ..Default::default()
        };
        assert!(c.resolve(&ctx).is_none());
    }

    #[test]
    fn match_keys_are_anded() {
        let c: RoutingConfig = toml::from_str(
            r#"
[[route]]
match = { linear_team = "OFF", label = "herd" }
workspace = "w"
repo = "/tmp"
runtime = "claude"
"#,
        )
        .unwrap();
        let team_only = RouteContext {
            linear_team: Some("OFF".into()),
            ..Default::default()
        };
        assert!(c.resolve(&team_only).is_none());
        let both = RouteContext {
            linear_team: Some("OFF".into()),
            labels: vec!["herd".into()],
            ..Default::default()
        };
        assert!(c.resolve(&both).is_some());
    }

    #[test]
    fn unknown_runtime_is_rejected_with_the_known_kinds() {
        // The impl spec's own example says `claude-code`, which is not a herdr
        // kind; we accept it as an alias but reject genuine typos.
        let c: RoutingConfig = toml::from_str(
            r#"
[[route]]
match = { label = "x" }
workspace = "w"
repo = "/tmp"
runtime = "claude-codex"
"#,
        )
        .unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("not a comet harness"), "{err}");
    }

    #[test]
    fn runtime_aliases_map_onto_comet_harnesses() {
        use comet_proto::HarnessId;
        assert_eq!(
            harness_for_runtime("claude-code"),
            Some(HarnessId::ClaudeCode)
        );
        assert_eq!(harness_for_runtime("claude"), Some(HarnessId::ClaudeCode));
        assert_eq!(harness_for_runtime("codex"), Some(HarnessId::Codex));
        assert_eq!(harness_for_runtime("nonesuch"), None);
    }

    #[test]
    fn catch_all_route_must_be_last() {
        let c: RoutingConfig = toml::from_str(
            r#"
[[route]]
workspace = "catchall"
repo = "/tmp"
runtime = "claude"

[[route]]
match = { label = "fintech" }
workspace = "fintech"
repo = "/tmp"
runtime = "claude"
"#,
        )
        .unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("would shadow"), "{err}");
    }

    #[test]
    fn interpolation_fills_known_keys_and_preserves_typos() {
        let mut v = BTreeMap::new();
        v.insert("title", "Add retry".to_string());
        v.insert("identifier", "LIN-145".to_string());
        let out = interpolate("{identifier}: {title} [{nope}]", &v);
        assert_eq!(out, "LIN-145: Add retry [{nope}]");
    }

    #[test]
    fn branch_template_renders_the_spec_default() {
        let mut v = BTreeMap::new();
        v.insert("identifier_lower", "lin-145".to_string());
        assert_eq!(interpolate("board/{identifier_lower}", &v), "board/lin-145");
    }

    #[test]
    fn home_paths_shorten_for_display() {
        // An absolute plugin path is long enough to be truncated on an 80-cell
        // row exactly where the filename is.
        unsafe { std::env::set_var("HOME", "/Users/x") };
        assert_eq!(
            shorten_home(Path::new("/Users/x/.comet-native/board/.env")),
            "~/.comet-native/board/.env"
        );
        assert_eq!(shorten_home(Path::new("/etc/hosts")), "/etc/hosts");
    }

    #[test]
    fn slugify_makes_branch_safe_text() {
        assert_eq!(
            slugify("Add retry to Altinn poller"),
            "add-retry-to-altinn-poller"
        );
        assert_eq!(slugify("LIN-145"), "lin-145");
        assert_eq!(slugify("  weird///chars  "), "weird-chars");
    }

    /// The file people are told to copy has to be a file that loads.
    #[test]
    fn the_shipped_example_config_parses_and_validates() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("routing.example.toml");
        let c = RoutingConfig::load(&path).unwrap();
        assert_eq!(c.routes.len(), 3);
        // `review_state` ships commented out: the example must not turn on a
        // transition against a state the reader's workspace may not have.
        assert!(c.linear.review_state.is_none());
        // Every runtime the example names has to be one comet will accept —
        // including the non-claude one, which is there precisely because it was
        // supported-but-never-exercised for the board's first two dozen
        // attempts (AGE-26).
        for r in &c.routes {
            assert!(
                harness_for_runtime(&r.runtime).is_some(),
                "example route runtime `{}` maps to no comet harness",
                r.runtime
            );
        }
        assert!(
            c.routes.iter().any(|r| r.runtime != "claude-code"),
            "the example should show that `runtime` is not decoration"
        );
    }

    #[test]
    fn the_review_state_is_unset_unless_named() {
        // Unset means no transition at all, which is what a workflow with no
        // review state needs — so it cannot have a default.
        assert!(cfg().linear.review_state.is_none());
        let c: RoutingConfig = toml::from_str(
            r#"
[linear]
review_state = "In Review"
"#,
        )
        .unwrap();
        assert_eq!(c.linear.review_state.as_deref(), Some("In Review"));
    }

    // ---- per-repo github settings --------------------------------------

    fn github(text: &str) -> RoutingConfig {
        let c: RoutingConfig = toml::from_str(text).unwrap();
        c.validate().unwrap();
        c
    }

    #[test]
    fn a_repo_can_be_polled_for_less_than_the_global_list() {
        // The bug: one global `labels` asks every repo the same question.
        // Adopting a repo whose whole roadmap is open issues put 83 rows on a
        // board that had 26 — because `labels = []` means "every open issue".
        let c = github(
            r#"
[github]
repos = ["Florin-AS/Tally", "bredebjorhovd/itsm-agent"]
labels = []

[[github.repo]]
name = "bredebjorhovd/itsm-agent"
labels = ["release-a"]
"#,
        );
        assert_eq!(
            c.github.labels_for("bredebjorhovd/itsm-agent"),
            ["release-a"]
        );
        // The repo that wanted "everything" still gets it.
        assert!(c.github.labels_for("Florin-AS/Tally").is_empty());
    }

    #[test]
    fn an_absent_repo_table_falls_back_and_an_empty_list_does_not() {
        // Absent and empty are different answers, and both are needed: a global
        // filter has to be widenable per repo as well as narrowable.
        let c = github(
            r#"
[github]
repos = ["o/curated", "o/backlog", "o/everything"]
labels = ["herd"]

[[github.repo]]
name = "o/backlog"
labels = ["release-a", "release-b"]

[[github.repo]]
name = "o/everything"
labels = []
"#,
        );
        assert_eq!(c.github.labels_for("o/curated"), ["herd"], "falls back");
        assert_eq!(c.github.labels_for("o/backlog"), ["release-a", "release-b"]);
        assert!(
            c.github.labels_for("o/everything").is_empty(),
            "an empty list is `everything`, not `fall back to herd`"
        );
    }

    #[test]
    fn a_repo_table_is_matched_however_it_is_cased() {
        // `[github] repos` is matched case-insensitively everywhere else, and a
        // filter that silently missed on `Tally` vs `tally` would be the same
        // class of bug all over again.
        let c = github(
            "[github]\nrepos = [\"Florin-AS/Tally\"]\n\n[[github.repo]]\nname = \"florin-as/tally\"\nlabels = [\"herd\"]\n",
        );
        assert_eq!(c.github.labels_for("Florin-AS/Tally"), ["herd"]);
    }

    #[test]
    fn a_repo_table_for_a_repo_that_is_not_polled_is_refused() {
        // Settings that apply to nothing, silently, is precisely what the table
        // was added to stop.
        let c: RoutingConfig = toml::from_str(
            "[github]\nrepos = [\"o/a\"]\n\n[[github.repo]]\nname = \"o/typo\"\nlabels = [\"x\"]\n",
        )
        .unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("not in `[github] repos`"), "{err}");
        assert!(err.contains("o/typo"), "it has to name the offender: {err}");
    }

    #[test]
    fn two_tables_for_one_repo_are_refused() {
        let c: RoutingConfig = toml::from_str(
            "[github]\nrepos = [\"o/a\"]\n\n[[github.repo]]\nname = \"o/a\"\nlabels = [\"x\"]\n\n[[github.repo]]\nname = \"o/a\"\nlabels = [\"y\"]\n",
        )
        .unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("appears twice"), "{err}");
    }

    #[test]
    fn a_config_with_no_repo_tables_at_all_behaves_as_it_always_did() {
        let c = github("[github]\nrepos = [\"o/a\"]\nlabels = [\"herd\"]\n");
        assert!(c.github.per_repo.is_empty());
        assert_eq!(c.github.labels_for("o/a"), ["herd"]);
        assert_eq!(c.github.labels_for("o/never-heard-of-it"), ["herd"]);
    }

    // ---- AGE-23: one writeback flag cannot answer for every repo ---------

    #[test]
    fn a_repo_can_be_read_only_while_the_rest_are_written_to() {
        // The bug: one global flag is set by its riskiest repo. Off, and the
        // board closes nothing anywhere; on, and it comments on production.
        let c = github(
            r#"
[github]
repos = ["bredebjorhovd/OIOS", "Florin-AS/Tally"]
writeback = true

[[github.repo]]
name = "Florin-AS/Tally"
writeback = false
"#,
        );
        assert!(c.github.writeback_for("bredebjorhovd/OIOS"));
        assert!(!c.github.writeback_for("Florin-AS/Tally"));
        assert_eq!(c.github.writeback_repos(), ["bredebjorhovd/OIOS"]);
        assert_eq!(c.github.read_only_repos(), ["Florin-AS/Tally"]);
    }

    #[test]
    fn a_repo_can_opt_in_while_the_global_flag_stays_off() {
        // The override has to go both ways, or the safe global default can only
        // be escaped by making every repo writable at once.
        let c = github(
            r#"
[github]
repos = ["o/mine", "o/theirs"]

[[github.repo]]
name = "o/mine"
writeback = true
"#,
        );
        assert!(!c.github.writeback, "the global flag is still off");
        assert!(c.github.writeback_for("o/mine"));
        assert!(!c.github.writeback_for("o/theirs"));
        assert_eq!(c.github.writeback_repos(), ["o/mine"]);
    }

    #[test]
    fn a_repo_table_without_writeback_falls_back_to_the_global_flag() {
        // A table written for `labels` must not quietly turn writeback off for
        // the repo it narrows — absent is "no answer here", not "no".
        let c = github(
            r#"
[github]
repos = ["o/a"]
writeback = true

[[github.repo]]
name = "o/a"
labels = ["release-a"]
"#,
        );
        assert!(c.github.settings_for("o/a").is_some());
        assert!(c.github.writeback_for("o/a"));
        assert_eq!(c.github.writeback_repos(), ["o/a"]);
    }

    #[test]
    fn a_repos_own_writeback_is_matched_however_it_is_cased() {
        // The repo comes from a task id, which carries whatever casing GitHub
        // returned; missing on `Tally` vs `tally` here would write to the one
        // repo the operator excluded.
        let c = github(
            "[github]\nrepos = [\"Florin-AS/Tally\"]\nwriteback = true\n\n\
             [[github.repo]]\nname = \"florin-as/tally\"\nwriteback = false\n",
        );
        assert!(!c.github.writeback_for("Florin-AS/Tally"));
        assert!(c.github.writeback_repos().is_empty());
    }

    #[test]
    fn writeback_defaults_to_off_for_every_repo() {
        let c = github("[github]\nrepos = [\"o/a\", \"o/b\"]\n");
        assert!(!c.github.writeback_for("o/a"));
        assert!(c.github.writeback_repos().is_empty());
        assert_eq!(c.github.read_only_repos(), ["o/a", "o/b"]);
    }

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration_secs("30s"), Some(30));
        assert_eq!(parse_duration_secs("5m"), Some(300));
        assert_eq!(parse_duration_secs("1h"), Some(3600));
        assert_eq!(parse_duration_secs("90"), Some(90));
        assert_eq!(parse_duration_secs(""), None);
    }

    // ---- the wall-clock cap (gh#70) --------------------------------------

    #[test]
    fn attempts_are_capped_at_two_hours_unless_told_otherwise() {
        // The default has to be a real number, not "unlimited": before this
        // existed, nothing bounded a running attempt at all.
        let c = RoutingConfig::default();
        assert_eq!(c.max_duration_secs(None), Some(7200));
    }

    #[test]
    fn a_route_sets_its_own_cap_over_the_default() {
        // The answer is a property of the work: the monorepo-refactor route is
        // not the typo route, and one number would be set by whichever runs
        // longest — leaving every other route unbounded.
        let c = github(
            r#"
[defaults]
max_duration = "45m"

[[route]]
match = { label = "big" }
workspace = "w"
repo = "/tmp"
runtime = "claude"
max_duration = "6h"

[[route]]
match = { label = "small" }
workspace = "w"
repo = "/tmp"
runtime = "claude"
"#,
        );
        assert_eq!(c.max_duration_secs(Some(&c.routes[0])), Some(21_600));
        assert_eq!(c.max_duration_secs(Some(&c.routes[1])), Some(2_700));
        // A route that has since been deleted from under a running attempt
        // falls back to the default, never to "unlimited".
        assert_eq!(c.max_duration_secs(None), Some(2_700));
    }

    #[test]
    fn off_is_how_the_cap_is_removed() {
        for spelling in ["off", "OFF", "none", "never", "0"] {
            let c = github(&format!("[defaults]\nmax_duration = \"{spelling}\"\n"));
            assert_eq!(c.max_duration_secs(None), None, "{spelling}");
        }
    }

    #[test]
    fn a_cap_shorter_than_a_minute_is_raised_to_one() {
        // A cap that expires before the first turn finishes would fail every
        // dispatch on the route.
        assert_eq!(parse_max_duration("5s"), Ok(Some(60)));
        assert_eq!(parse_max_duration("90s"), Ok(Some(90)));
    }

    #[test]
    fn a_mistyped_cap_is_refused_rather_than_read_as_unlimited() {
        // The two look identical on the board and only one is what anybody
        // meant — so it is a config error, like an unknown runtime.
        let c: RoutingConfig = toml::from_str(
            "[[route]]\nmatch = { label = \"x\" }\nworkspace = \"w\"\nrepo = \"/tmp\"\n\
             runtime = \"claude\"\nmax_duration = \"two hours\"\n",
        )
        .unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("max_duration"), "{err}");
        assert!(err.contains("two hours"), "it names the offender: {err}");

        let c: RoutingConfig = toml::from_str("[defaults]\nmax_duration = \"forever\"\n").unwrap();
        assert!(
            c.validate()
                .unwrap_err()
                .to_string()
                .contains("[defaults] max_duration")
        );
    }

    #[test]
    fn interval_has_a_floor() {
        let s = SyncConfig {
            interval: "0s".into(),
            labels: vec![],
        };
        assert_eq!(s.interval_secs(), 5);
    }

    #[test]
    fn credentials_are_re_read_after_the_env_file_is_edited() {
        fn no_inherited(_: &str) -> std::result::Result<String, std::env::VarError> {
            Err(std::env::VarError::NotPresent)
        }

        let dir = std::env::temp_dir().join(format!(
            "hb-credentials-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let paths = Paths {
            config_dir: dir.clone(),
            state_dir: dir.clone(),
        };
        std::fs::write(paths.env_file(), "LINEAR_API_KEY=first\n").unwrap();
        let first = Credentials::load_with(&paths, no_inherited);
        assert_eq!(first.linear_api_key.as_deref(), Some("first"));
        assert_eq!(first.github_token, None);

        std::fs::write(
            paths.env_file(),
            "LINEAR_API_KEY=second\nGITHUB_TOKEN=github\n",
        )
        .unwrap();
        let edited = Credentials::load_with(&paths, no_inherited);
        assert_eq!(edited.linear_api_key.as_deref(), Some("second"));
        assert_eq!(edited.github_token.as_deref(), Some("github"));

        std::fs::write(paths.env_file(), "GITHUB_TOKEN=github\n").unwrap();
        let removed = Credentials::load_with(&paths, no_inherited);
        assert_eq!(removed.linear_api_key, None);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn inherited_credentials_override_the_env_file() {
        let dir = std::env::temp_dir().join(format!(
            "hb-credential-precedence-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let paths = Paths {
            config_dir: dir.clone(),
            state_dir: dir.clone(),
        };
        std::fs::write(
            paths.env_file(),
            "LINEAR_API_KEY=file-linear\nGITHUB_TOKEN=file-github\n",
        )
        .unwrap();

        let credentials = Credentials::load_with(&paths, |key| match key {
            "LINEAR_API_KEY" => Ok("shell-linear".to_string()),
            "GITHUB_TOKEN" => Ok(String::new()),
            _ => Err(std::env::VarError::NotPresent),
        });
        assert_eq!(credentials.linear_api_key.as_deref(), Some("shell-linear"));
        assert_eq!(credentials.github_token, None);

        let _ = std::fs::remove_dir_all(dir);
    }

    fn creds(env: &str) -> Credentials {
        let dir = std::env::temp_dir().join(format!(
            "hb-gh-auth-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let paths = Paths {
            config_dir: dir.clone(),
            state_dir: dir.clone(),
        };
        std::fs::write(paths.env_file(), env).unwrap();
        let c = Credentials::load_with(&paths, |_| Err(std::env::VarError::NotPresent));
        let _ = std::fs::remove_dir_all(dir);
        c
    }

    /// A skipped stage and a board nobody ever configured have to look
    /// identical (gh#96). The box wizard writes `LINEAR_API_KEY=` when the
    /// stage is skipped with Enter, and an empty string that reads as
    /// "configured" turns a deliberate skip into a credential the board tries
    /// to authenticate with — and, for the App pair, into a "half configured"
    /// failure over two keys nobody set.
    #[test]
    fn an_empty_value_reads_as_never_configured() {
        let c =
            creds("LINEAR_API_KEY=\nGITHUB_TOKEN=\nGITHUB_APP_ID=\nGITHUB_APP_PRIVATE_KEY_PATH=\n");
        assert_eq!(c.linear_api_key, None);
        assert_eq!(c.github_auth(), GithubAuth::None);
        assert_eq!(c.github_app_half_configured(), None);

        // `KEY=""` is the same skip, written by a wizard that quotes.
        assert_eq!(creds("LINEAR_API_KEY=\"\"\n").linear_api_key, None);

        // And one real key beside a skipped one is still one real key.
        let c = creds("LINEAR_API_KEY=\nGITHUB_TOKEN=ghp_real\n");
        assert_eq!(c.linear_api_key, None);
        assert_eq!(c.github_auth(), GithubAuth::Token("ghp_real".into()));
    }

    #[test]
    fn a_configured_app_wins_over_a_personal_access_token() {
        // Both present is the migration state: the App is registered and the
        // old token is still in the file. The App is the newer intent.
        let c = creds(
            "GITHUB_TOKEN=ghp_old\nGITHUB_APP_ID=123456\n\
             GITHUB_APP_PRIVATE_KEY_PATH=/etc/comet/app.pem\n",
        );
        assert_eq!(
            c.github_auth(),
            GithubAuth::App {
                app_id: "123456".into(),
                key_path: PathBuf::from("/etc/comet/app.pem"),
            }
        );
        assert_eq!(c.github_app_half_configured(), None);
    }

    #[test]
    fn a_personal_access_token_keeps_working_with_no_app_configured() {
        // The regression guard for every board already deployed: nobody is
        // pushed onto registering an App by this change.
        assert_eq!(
            creds("GITHUB_TOKEN=ghp_only\n").github_auth(),
            GithubAuth::Token("ghp_only".into())
        );
        assert_eq!(creds("").github_auth(), GithubAuth::None);
    }

    #[test]
    fn half_an_app_falls_back_to_the_token_and_is_named_as_a_mistake() {
        // The quiet failure: the board keeps working, on the wrong identity,
        // with no error anywhere. `doctor` reads this to say so.
        let c = creds("GITHUB_TOKEN=ghp_old\nGITHUB_APP_ID=123456\n");
        assert_eq!(c.github_auth(), GithubAuth::Token("ghp_old".into()));
        assert_eq!(
            c.github_app_half_configured(),
            Some("GITHUB_APP_PRIVATE_KEY_PATH")
        );

        let c = creds("GITHUB_APP_PRIVATE_KEY_PATH=/etc/comet/app.pem\n");
        assert_eq!(c.github_auth(), GithubAuth::None);
        assert_eq!(c.github_app_half_configured(), Some("GITHUB_APP_ID"));
    }
}
