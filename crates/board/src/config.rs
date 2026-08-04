//! Board directories, `.env` secrets, and `routing.toml`.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
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
    pub fn worktree_root(&self) -> PathBuf {
        self.state_dir.join("wt")
    }
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

#[derive(Debug, Clone, Default, Deserialize)]
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
#[derive(Debug, Clone, Default, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
pub struct Defaults {
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_per_workspace: usize,
    #[serde(default = "default_branch_template")]
    pub branch_template: String,
    /// Surface a notification when released work settles.
    ///
    /// A conversational orchestrator cannot be woken — it only gets a turn when
    /// something prompts it — so the operator is the one who has to notice.
    /// Off means noticing is entirely on you.
    #[serde(default = "default_true")]
    pub notify: bool,
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

impl Default for Defaults {
    fn default() -> Self {
        Defaults {
            max_concurrent_per_workspace: default_max_concurrent(),
            branch_template: default_branch_template(),
            notify: true,
            notify_dispatcher: false,
            new_source: default_new_source(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
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
#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Default, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
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
    #[serde(default)]
    pub max_concurrent: Option<usize>,
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

#[derive(Debug, Clone, Default, Deserialize)]
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
        for (i, r) in self.routes.iter().enumerate() {
            if r.match_.is_empty() {
                // A catch-all route is legal and useful, but it must be last or
                // it silently shadows everything after it.
                if i + 1 != self.routes.len() {
                    bail!(
                        "route {} ({}) has an empty `match` but is not last; \
                         first matching route wins, so it would shadow the {} route(s) after it",
                        i + 1,
                        r.display_name(),
                        self.routes.len() - i - 1
                    );
                }
            }
            if harness_for_runtime(&r.runtime).is_none() {
                bail!(
                    "route {} ({}) has runtime `{}`, which is not a comet harness. \
                     Known runtimes: {}",
                    i + 1,
                    r.display_name(),
                    r.runtime,
                    RUNTIME_NAMES.join(", ")
                );
            }
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
                bail!(
                    "[[github.repo]] name = \"{}\" is not in `[github] repos`, so nothing \
                     would ever use it. Add it to `repos`, or correct the name.",
                    r.name
                );
            }
            if self.github.per_repo[..i]
                .iter()
                .any(|earlier| earlier.name.eq_ignore_ascii_case(&r.name))
            {
                bail!(
                    "[[github.repo]] name = \"{}\" appears twice; only the first would \
                     be used, so the second is settings that do nothing",
                    r.name
                );
            }
        }
        Ok(())
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

    pub fn max_concurrent(&self, route: &Route) -> usize {
        route
            .max_concurrent
            .unwrap_or(self.defaults.max_concurrent_per_workspace)
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
