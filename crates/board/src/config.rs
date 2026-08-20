//! Board directories, `.env` secrets, and `routing.toml`.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::runtime::{RUNTIME_NAMES, harness_for_runtime};

/// Where the board keeps things: under comet's own data dir, beside the
/// engine's stores.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
}

/// The two variables that name a board from outside — see [`Paths::discover`].
///
/// Named once so the guard test (`crates/board/tests/env_isolation.rs`) and the
/// dispatched-agent environment ([`crate::git_credentials::agent_env`]) cannot
/// spell them differently than the resolution does.
pub const CONFIG_DIR_ENV: &str = "COMET_BOARD_CONFIG_DIR";
pub const STATE_DIR_ENV: &str = "COMET_BOARD_STATE_DIR";

impl Paths {
    /// The board this *process* was pointed at, for a `comet-board` that was
    /// handed no data dir — the CLI, and above all the `git-askpass` /
    /// `credential` helpers git spawns with none of their parent's arguments.
    ///
    /// The only resolution that reads [`CONFIG_DIR_ENV`] / [`STATE_DIR_ENV`],
    /// and the only one that may (gh#190). Those variables exist for exactly
    /// this: the engine exports its *already-resolved* pair into every
    /// dispatched agent's environment ([`crate::git_credentials::agent_env`]) so
    /// a `comet-board` started down there attaches to the board that dispatched
    /// it instead of re-deriving a different one. They are an answer to "which
    /// board is this shell's", never an instruction to relocate a board — so
    /// nothing that already knows its data dir consults them, and no library
    /// call can inherit them from whatever shell it happens to be running in.
    pub fn discover() -> Result<Paths> {
        let base = data_dir().join("board");
        let config_dir = env_dir(CONFIG_DIR_ENV).unwrap_or_else(|| base.clone());
        let state_dir = env_dir(STATE_DIR_ENV).unwrap_or_else(|| base.join("state"));
        Self::at(config_dir, state_dir)
    }

    /// The board directories under an already-resolved engine data dir — the
    /// engine's board service passes its own `EngineConfig::data_dir` here so
    /// config precedence cannot diverge between the two.
    ///
    /// Deliberately pure: the caller's directory is the answer, and no
    /// environment variable may overrule it (gh#190). It used to honour
    /// [`CONFIG_DIR_ENV`] / [`STATE_DIR_ENV`], which is how a *test* handed a
    /// tempdir — and a dev engine started under `COMET_DATA_DIR=/some/scratch`
    /// — opened the box's live `board.db` and logged into its `syncd.log`
    /// instead: both run inside a dispatched agent, whose environment carries
    /// that pair by design. An operator relocating a whole install moves
    /// `COMET_DATA_DIR`, which this follows.
    pub fn under(data_dir: &Path) -> Result<Paths> {
        let base = data_dir.join("board");
        let state = base.join("state");
        Self::at(base, state)
    }

    fn at(config_dir: PathBuf, state_dir: PathBuf) -> Result<Paths> {
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

/// One directory-naming variable, or `None` when it is unset or empty.
fn env_dir(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The engine's data dir — `$COMET_DATA_DIR`, else `~/.comet-native`.
///
/// The board's own directories hang off it (`board/`, and `board/state/`) on
/// purpose: one directory to back up, and its lifetime is already the
/// engine's. `COMET_DATA_DIR` follows the engine's dev-mode override.
pub fn data_dir() -> PathBuf {
    match std::env::var("COMET_DATA_DIR") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".comet-native")
        }
    }
}

/// Where a clone the engine makes for itself lands — `{data_dir}/repos`, and
/// the parent an onboarding clone uses when it was given no `--dir`.
///
/// `Repos::clone_root` is the same path read from inside the engine, off the
/// data dir it was constructed with. This is it from outside, for the code that
/// has to *say* where a checkout would go — the repair line a route with no
/// space carries (gh#342) has to name the `--dir` only when the path is not
/// this one.
pub fn clone_root() -> PathBuf {
    data_dir().join("repos")
}

/// The per-slot agent config dirs a dispatch runs under — `{data_dir}/accounts/*`
/// (gh#59), one per agent-account slot that has ever been materialized.
///
/// The engine's `agent_accounts` owns that layout and resolves it from its own
/// `data_dir`; this reads the same place from outside, because `doctor` has to
/// answer a question about dirs it does not create — whether the skill the
/// agents in them see is this binary's (gh#133). Missing root, or none
/// materialized yet, is an empty list and not an error: a board whose
/// dispatches all run on the box's own login has no slots at all.
pub fn agent_account_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(data_dir().join("accounts"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        // `.login-*` are throwaway CODEX_HOME dirs from a login flow in
        // progress; nothing dispatches into one.
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs
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
    pub(crate) github_token: Option<String>,
    /// The GitHub App's numeric id, and the PEM of its private key on disk
    /// (gh#58). Both or neither: one alone is a half-configured App, which
    /// leaves the board silently on whatever `GITHUB_TOKEN` says.
    pub(crate) github_app_id: Option<String>,
    pub(crate) github_app_key_path: Option<String>,
    /// A member's *own* GitHub token, keyed by [`user_token_key`] of their
    /// login (gh#369) — `GITHUB_USER_TOKEN_ANA` for `@ana`.
    ///
    /// The one credential here that does not belong to the board. It is read
    /// for exactly one call — submitting a review — because GitHub refuses
    /// `APPROVE` on a pull request the caller's own identity opened, and the
    /// board's identity opened every dispatched one. A verdict cast under the
    /// reviewer's token is a verdict GitHub takes, and it carries their name
    /// on the pull request, which is the only version of an approval that
    /// means anything to somebody reading it later.
    ///
    /// Keyed by login rather than by the sign-in email the `[users]` map keys
    /// on: an email is not a legal environment variable name, and the login is
    /// the half of the map GitHub actually recognises.
    pub(crate) user_tokens: BTreeMap<String, String>,
}

/// The `.env` prefix under which a member keeps their own GitHub token
/// (gh#369).
pub const USER_TOKEN_PREFIX: &str = "GITHUB_USER_TOKEN_";

/// The `.env` key holding `login`'s own token: the login, uppercased, with the
/// one character a login may hold and an environment variable may not — the
/// hyphen — written `_`.
///
/// Injective, which is what stops two teammates sharing one key: a GitHub login
/// is ASCII alphanumerics and hyphens ([`crate::members::is_github_login`]), so
/// `a_b` is not a login anybody can have and `a-b` is the only thing that can
/// produce `A_B`.
pub fn user_token_env(login: &str) -> String {
    format!("{USER_TOKEN_PREFIX}{}", user_token_key(login))
}

/// The map key [`Credentials::user_token`] looks a login up under — the part of
/// [`user_token_env`] after the prefix, which is also what a `.env` key that
/// carries the prefix is stripped down to.
fn user_token_key(login: &str) -> String {
    login.trim().to_ascii_uppercase().replace('-', "_")
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
        Self::load_with_env(paths, |key| std::env::var(key), std::env::vars())
    }

    /// A read with a lookup and no shell environment behind it — the shape the
    /// tests want, where "what the shell has" is exactly what they pass in.
    #[cfg(test)]
    fn load_with<F>(paths: &Paths, inherited: F) -> Credentials
    where
        F: Fn(&str) -> std::result::Result<String, std::env::VarError>,
    {
        Self::load_with_env(paths, inherited, [])
    }

    /// As [`Credentials::load_with`], with the shell's whole environment rather
    /// than a lookup.
    ///
    /// The member tokens are the reason: they are named after whoever holds
    /// them ([`user_token_env`]), so there is no fixed key to ask for and the
    /// set has to be walked. Everything else keeps asking by name.
    fn load_with_env<F, I>(paths: &Paths, inherited: F, shell: I) -> Credentials
    where
        F: Fn(&str) -> std::result::Result<String, std::env::VarError>,
        I: IntoIterator<Item = (String, String)>,
    {
        Credentials {
            github_token: credential(paths, "GITHUB_TOKEN", &inherited),
            github_app_id: credential(paths, "GITHUB_APP_ID", &inherited),
            github_app_key_path: credential(paths, "GITHUB_APP_PRIVATE_KEY_PATH", &inherited),
            user_tokens: user_tokens(paths, shell),
        }
    }

    /// The token `login` keeps on this box, when they keep one (gh#369).
    ///
    /// `None` is the ordinary case and not a fault: a member with no token of
    /// their own reviews exactly as the board did before this existed, which
    /// gh#365 made a safe path rather than a failed submission.
    pub fn user_token(&self, login: &str) -> Option<&str> {
        self.user_tokens
            .get(&user_token_key(login))
            .map(String::as_str)
    }

    /// A box holding one member's review token and nothing else — the
    /// arrangement gh#369's tests are about, without a `.env` on disk.
    #[cfg(test)]
    pub(crate) fn with_user_token(login: &str, token: &str) -> Credentials {
        Credentials {
            user_tokens: BTreeMap::from([(user_token_key(login), token.to_string())]),
            ..Default::default()
        }
    }

    /// Which members this box holds a review credential for, by login. For
    /// `doctor` and `member list`, which say who can cast a verdict GitHub
    /// will take — never the tokens themselves.
    pub fn user_token_logins(&self) -> impl Iterator<Item = &str> {
        self.user_tokens.keys().map(String::as_str)
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

/// Every `GITHUB_USER_TOKEN_*` this box holds, keyed by [`user_token_key`].
///
/// The same precedence [`credential`] gives one key, applied to a set: the file
/// first, the shell over it, and a shell variable that is explicitly empty
/// takes the file's value away rather than shadowing it with nothing. A key
/// with nothing after the prefix is skipped — it names no login, so no lookup
/// could ever reach it.
fn user_tokens<I>(paths: &Paths, shell: I) -> BTreeMap<String, String>
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut out = BTreeMap::new();
    let mut put = |name: String, value: String| {
        let Some(key) = name
            .strip_prefix(USER_TOKEN_PREFIX)
            .filter(|k| !k.is_empty())
        else {
            return;
        };
        match value.trim().is_empty() {
            true => drop(out.remove(key)),
            false => drop(out.insert(key.to_string(), value.trim().to_string())),
        }
    };
    if let Ok(entries) = dotenvy::from_path_iter(paths.env_file()) {
        for (name, value) in entries.flatten() {
            put(name, value);
        }
    }
    for (name, value) in shell {
        put(name, value);
    }
    out
}

pub fn github_token(paths: &Paths) -> Option<String> {
    Credentials::load(paths).github_token
}

/// The GitHub credential mode in force, read off `.env`.
pub fn github_auth(paths: &Paths) -> GithubAuth {
    Credentials::load(paths).github_auth()
}

/// `routing.toml`, parsed.
///
/// `PartialEq` is load-bearing, here and on every type below it: the board loop
/// decides whether to rebuild itself by comparing the config it is running
/// against the config on disk ([`crate::sync::SyncEngine::reload_if_configuration_changed`]).
/// A derived comparison cannot fall behind the struct it is derived from, which
/// a hand-picked list of fields did — for two releases every `[defaults]` key
/// was invisible to the loop (gh#189). Adding a field here needs nothing; taking
/// the derive away would silently restore the bug.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
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
    pub adopt: AdoptConfig,
    /// Who the people driving this board are, on GitHub (gh#107).
    ///
    /// Keyed by the identity a dispatch arrives with — the email the
    /// dispatching frontend reports its signed-in user as (gh#74) — and valued
    /// with that person's git author address, ideally the GitHub noreply form
    /// (`<id>+<login>@users.noreply.github.com`, from
    /// <https://github.com/settings/emails>):
    ///
    /// ```toml
    /// [users]
    /// "ana@example.com" = "22494697+ana@users.noreply.github.com"
    /// "sam@example.com" = "Sam Ito <8134+samito@users.noreply.github.com>"
    /// ```
    ///
    /// A file, not a directory service, because a two-person box does not need
    /// one — and because the mapping is a decision (which GitHub account is
    /// this person?) that nothing on the box can answer on its own: the board's
    /// App may not read anybody's email addresses.
    ///
    /// Empty is the single-operator default: every dispatch commits as the box.
    #[serde(default)]
    pub users: BTreeMap<String, String>,
    /// What each agent-account slot's subscription costs its owner (gh#182),
    /// keyed by the slot id `doctor` lists — or by the email on it, when that
    /// is the spelling at hand:
    ///
    /// ```toml
    /// [account."8f2c1d0a7b6e4539"]
    /// email = "brede@tally.no"
    /// plan = "Claude Max 20x"
    /// monthly_usd = 200
    /// ```
    ///
    /// Beside the slot it describes rather than in `[defaults]`, because it is
    /// not the board's fact: rates are what the board knows about the meter,
    /// and this is what one person pays for one plan. On a box carrying several
    /// teammates' slots (gh#59) a single board-wide "subscription cost" would
    /// be adding up other people's bills and calling the sum the board's spend.
    ///
    /// Nothing here is discoverable — comet never sees anybody's invoice — so
    /// an unconfigured slot is reported as *unknown*, never as free. Which is
    /// why the Accounts settings page asks for it beside the login it belongs
    /// to and writes it here ([`crate::routes::Edit::Account`], gh#178): it is
    /// the one line of this file no amount of probing could fill in.
    #[serde(default, rename = "account")]
    pub accounts: BTreeMap<String, AccountConfig>,
    /// Auto-pick rules (gh#490): saved, enableable policies that dispatch
    /// eligible labeled tasks without a keypress. In this file rather than in
    /// `board.db` because a rule is *policy* — the same kind of decision a
    /// route is — and everything that edits policy here (the settings page,
    /// `$EDITOR` over ssh, the validating writer) already exists. Enabling one
    /// is the explicit human authorization for every dispatch it later makes.
    #[serde(default, rename = "automation")]
    pub automations: Vec<Automation>,
}

/// One agent account's plan, as its operator wrote it down (gh#182).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AccountConfig {
    /// The login's email, when the key above is a slot id. What the board
    /// actually matches against: an attempt records whose subscription it
    /// spent, and that is an address, never a slot id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// What the plan is called, in the operator's words (`Claude Max 20x`,
    /// `Team seat`). Free text: comet has no list of plans, and inventing one
    /// would put words in somebody's mouth about their own bill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// What it costs per month, in US dollars. Pro-rated across the stats
    /// window it is read against — see [`crate::prices`].
    #[serde(default)]
    pub monthly_usd: comet_proto::view::rates::Usd,
}

/// What the board offers to adopt, and what it has been told to stop offering.
///
/// Only an exclusion list: adoption itself writes ordinary `[[route]]` and
/// `[github] repos` entries, because those are the config that already exists.
/// Ignoring has nowhere else to live — "I am only reading this repo" is not a
/// fact any other key can carry.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncConfig {
    /// Poll interval, e.g. `"30s"`.
    #[serde(default = "default_interval")]
    pub interval: String,
}

fn default_interval() -> String {
    "30s".into()
}

impl Default for SyncConfig {
    fn default() -> Self {
        SyncConfig {
            interval: default_interval(),
        }
    }
}

impl SyncConfig {
    /// Parse `30s` / `5m` / `90` (bare = seconds). Clamped to a sane floor so a
    /// typo cannot hammer the source.
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

/// Parse a `max_tool_failures` / `max_tool_calls` value into a per-turn count
/// (gh#270).
///
/// `Ok(None)` is *no guardrail*, said out loud — `off`, `none`, `never` or `0`.
/// An unparseable value is an `Err` for [`parse_max_duration`]'s reason: a typo
/// and "unlimited" look identical from the board, and only one of them is what
/// anybody meant. Anything below `floor` is raised to it — a cap that fires
/// before an agent has finished orienting would steer every run on the route,
/// which is how a guardrail gets turned off for good.
pub fn parse_turn_limit(s: &str, floor: u32) -> std::result::Result<Option<u32>, String> {
    let t = s.trim();
    if matches!(t.to_ascii_lowercase().as_str(), "off" | "none" | "never") {
        return Ok(None);
    }
    match t.parse::<u32>() {
        Ok(0) => Ok(None),
        Ok(n) => Ok(Some(n.max(floor))),
        Err(_) => Err(format!(
            "`{s}` is not a count; write it like `10`, `2000`, or `off`"
        )),
    }
}

/// Parse a `min_memory_headroom` value into the share of the box's memory that
/// must still be available before a dispatch may start (gh#533).
///
/// `Ok(None)` is *no memory gate*, said out loud — `off`, `none`, `never` or
/// `0%`. A percentage is the only other spelling: a byte count would have to be
/// re-decided every time the box is resized, and the whole point of the floor is
/// that it scales with `MemTotal`. Anything at or over 100% — or below zero —
/// is refused rather than clamped: a floor no box can ever be over is a board
/// that dispatches nothing, and finding that out at 02:00 is the failure this
/// guard exists to prevent, not to cause.
///
/// An unparseable value is an `Err` for [`parse_max_duration`]'s reason: a typo
/// and "no gate" look identical from the board, and only one of them is what
/// anybody meant.
pub fn parse_memory_headroom(s: &str) -> std::result::Result<Option<f64>, String> {
    let t = s.trim();
    if matches!(t.to_ascii_lowercase().as_str(), "off" | "none" | "never") {
        return Ok(None);
    }
    let number = t.strip_suffix('%').unwrap_or(t).trim();
    let Ok(n) = number.parse::<f64>() else {
        return Err(format!(
            "`{s}` is not a percentage; write it like `15%`, `25%`, or `off`"
        ));
    };
    if n == 0.0 {
        return Ok(None);
    }
    if !(0.0..100.0).contains(&n) {
        return Err(format!(
            "is `{n}%` — a floor below zero or at 100% or more is a board that can never \
             dispatch; write it like `15%` or `off`"
        ));
    }
    Ok(Some(n / 100.0))
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

/// Parse an `archive_chats` value (gh#139) — [`parse_retention`] plus the
/// spelling a shelf needs and a disk does not.
///
/// `on-settle` (also `immediately`, `now`) is `Some(0)`: archive as soon as the
/// task has left the board, with no window at all. A checkout is evidence you
/// might go back for, so a week of them is a week of insurance; a chat is a row
/// you are *shown*, and a finished row keeps its place in the list it is
/// cluttering for exactly as long as the window says. Nothing is lost either
/// way — archiving is not deleting.
///
/// A bare `0` is rejected rather than guessed at. It reads as "no window" here
/// and means "off — keep forever" in [`parse_retention`], and the two are
/// opposites; the error names both spellings.
pub fn parse_chat_retention(s: &str) -> std::result::Result<Option<u64>, String> {
    let t = s.trim().to_ascii_lowercase();
    if matches!(
        t.as_str(),
        "on-settle" | "on settle" | "immediately" | "now"
    ) {
        return Ok(Some(0));
    }
    if matches!(t.as_str(), "0" | "0s" | "0m" | "0h" | "0d") {
        return Err(format!(
            "`{s}` is ambiguous here — write `on-settle` to archive as soon as \
             the task leaves the board, or `off` to keep chats forever"
        ));
    }
    parse_retention(s).map_err(|_| {
        format!("`{s}` is not a duration; write it like `on-settle`, `2d`, `1w`, or `off`")
    })
}

/// Parse a `retain_build_output` value (gh#186) — [`parse_retention`] with the
/// same `on-settle` spelling [`parse_chat_retention`] needs, meaning the same
/// thing: no window at all.
///
/// It is the default here, and the reason is arithmetic. A checkout is 14 MB and
/// its `target/` is 20–36 GB; nothing reads the second one after the run ends,
/// and a box with a 150 GB disk cannot hold a week of them. `on-settle` sweeps
/// the build output as the *attempt* ends — not when the task leaves the board,
/// which is `retain_worktrees`'s much longer clock — so the only thing an
/// immediate sweep ever costs is one rebuild in the checkout, and only if
/// somebody comes back to it at all.
///
/// A duration (`2h`, `1d`) buys that rebuild back for a box with disk to spare.
/// `off` keeps every cache for as long as its checkout lives, which is what the
/// board did before this existed and what filled the disk. A bare `0` is
/// rejected for [`parse_chat_retention`]'s reason: it reads as "no window" here
/// and "keep forever" there, and guessing between opposites is worse than asking.
pub fn parse_build_retention(s: &str) -> std::result::Result<Option<u64>, String> {
    let t = s.trim().to_ascii_lowercase();
    if matches!(
        t.as_str(),
        "on-settle" | "on settle" | "immediately" | "now"
    ) {
        return Ok(Some(0));
    }
    if matches!(t.as_str(), "0" | "0s" | "0m" | "0h" | "0d") {
        return Err(format!(
            "`{s}` is ambiguous here — write `on-settle` to sweep build output as \
             soon as the attempt ends, or `off` to keep it for as long as the \
             checkout lives"
        ));
    }
    parse_retention(s).map_err(|_| {
        format!("`{s}` is not a duration; write it like `on-settle`, `2h`, `1d`, or `off`")
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Defaults {
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_per_workspace: usize,
    /// How much of the box's memory must still be available before a dispatch
    /// may start (gh#533) — a percentage of `MemTotal`, or `off`.
    ///
    /// The cap above counts *slots*, and a slot is not a memory budget: three
    /// slots of Next.js builds is not three slots of doc edits, and on
    /// 2026-08-19 a swapless 16G box sat at 3-of-3 heavy builds until the
    /// kernel's OOM killer reached inside the engine's unit (gh#526). This is
    /// the meter the slot count cannot be — measured on the box, at the moment
    /// of the dispatch, from what the last three agents actually left.
    ///
    /// 15% by default. Below it a dispatch is **deferred**, not refused: the
    /// board's word for eligible-but-held by a limit that time will lift
    /// (gh#490), which is exactly what a build finishing does to this one. The
    /// same reading also defers when the box is already stalling on memory
    /// whatever it claims is free — see [`crate::pressure`].
    ///
    /// `off` (or `0%`) is the escape hatch, and the honest spelling of what
    /// every board did before this existed. A box that cannot be measured at
    /// all — macOS, a kernel that does not answer — is `off` by construction:
    /// nothing is guessed, and no dispatch is ever held for a reading nobody
    /// took.
    #[serde(default = "default_memory_headroom")]
    pub min_memory_headroom: String,
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
    ///
    /// When this names a branch rather than the remote's default, the brief
    /// says so and tells the agent to open its pull request against it
    /// (gh#284): opening one is the agent's job, and `gh pr create` with no
    /// `--base` targets the repo default — which for a route based on
    /// `release-1.x` is a request to merge the release branch into `main`. See
    /// [`crate::dispatch::pr_base`].
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
    /// Tell the *agent* that released a task when its work settles or blocks,
    /// by queueing a message into the chat it dispatched from (AGE-25).
    ///
    /// **On by default since gh#165**, and first in the chain: this is the
    /// precise channel, prompting the one agent whose plan that task was a step
    /// in. It was off for a release on the grounds that "an orchestrator woken
    /// by every child it released cannot hold a train of thought" — true, and a
    /// description of the *other* channel, which delivers the whole board into
    /// one chat. A chat told about the two or three things it released is not
    /// the same volume as a chat told about everything.
    ///
    /// Turn it off for a board dispatched from chats that are you at a prompt
    /// rather than agents — [`notify`](Self::notify) is the channel for that
    /// reader — and every settle then falls to
    /// [`orchestrator_chat`](Self::orchestrator_chat), or to nobody.
    ///
    /// Independent of `notify`, because they are different audiences: this one
    /// never fires for operator-released work, which has no dispatcher.
    #[serde(default = "default_true")]
    pub notify_dispatcher: bool,
    /// The chat pinned as this board's orchestrator (gh#104): the one agent
    /// that hears what **nobody else can be told**.
    ///
    /// [`notify_dispatcher`](Self::notify_dispatcher) wakes the chat that
    /// released each task, which is the right audience whenever there is one.
    /// This is the address for the events where there is not, and gh#165 made
    /// that the whole of its job rather than a superset of the other channel:
    ///
    /// - **Work no agent released.** The board panel, the phone and a bare
    ///   `comet-board dispatch` record no dispatching chat, and that is most of
    ///   a solo operator's dispatches.
    /// - **A dispatcher that did not survive its child.** Attempts cap at two
    ///   hours and chats archive as their task settles, so the notice arriving
    ///   after the parent is gone is ordinary. It hops here instead of being
    ///   dropped.
    /// - **Events that belong to no attempt**, which is the duration cap's
    ///   warning: the attempt is still running, so no dispatcher is waiting on
    ///   a step that finished.
    ///
    /// What the dispatcher was told is not repeated here, and that is what
    /// makes a pin survivable on a busy board. Delivery is the same
    /// [`crate::runtime::Runtime`] path review delivery uses. One per board —
    /// re-pinning moves it — and unset (the default) is a board where those
    /// three cases reach nobody, which `doctor` says out loud.
    ///
    /// The value is a comet chat id. Nothing here can check that it names a
    /// live chat: the board core has no runtime at parse time, so a stale id is
    /// caught at delivery (the chat is gone; the log says so once) and named by
    /// `doctor`. Empty string reads as unset rather than as a chat called ""
    /// — see [`Defaults::orchestrator`].
    #[serde(default)]
    pub orchestrator_chat: Option<String>,
    /// Which tracker `comet-board new` writes to. `github` is the only
    /// supported value (gh#471); the key survives so configs that set it
    /// explicitly keep parsing.
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
    /// How many tool calls may fail *in a row* inside one turn before the board
    /// steers the agent, and then — at twice that — ends the run (gh#270).
    /// Overridden per route.
    ///
    /// The cap `max_duration` cannot be: an agent retrying the same failing
    /// command spends two hours' worth of tokens in the first ten minutes and
    /// the wall clock charges for all of it. Ten by default — high enough that
    /// a flaky test suite retried a few times is untouched, low enough that
    /// nothing legitimate reaches it, because ten failures in a row with
    /// nothing landing in between is not a technique.
    ///
    /// Counted per turn, and cleared outright by a single tool call that
    /// succeeds — so it can only ever fire on a run where nothing is landing.
    /// See [`crate::spin`], which also spends the doubling: this is the count
    /// for the *same* call failing, and assorted calls failing get twice the
    /// rope. `off` (or `0`) removes it, which is the honest spelling of what
    /// every board did before this existed.
    #[serde(default = "default_max_tool_failures")]
    pub max_tool_failures: String,
    /// How many tool calls one turn may make before the board steers, and then
    /// — at twice that — ends the run (gh#270). Overridden per route.
    ///
    /// The other half of a loop: a run that never fails and never finishes,
    /// re-reading the same file or re-planning the same step. Named for what it
    /// counts rather than for ForgeCode's `max_requests_per_turn`, whose figure
    /// this approximates — comet watches the harness from outside and sees tool
    /// calls, not the model requests behind them.
    ///
    /// Two thousand by default. That is far past any turn that is getting
    /// somewhere — most long dispatches make a few hundred — and far short of
    /// what a spinning agent reaches inside its wall clock. `off` (or `0`)
    /// removes it.
    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls: String,
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
    /// How long the *build output inside* a finished attempt's checkout is kept
    /// (gh#186) — `target/`, `node_modules/`, and the rest of
    /// [`crate::gc::BUILD_OUTPUT_DIRS`].
    ///
    /// A separate, much shorter clock than
    /// [`retain_worktrees`](Self::retain_worktrees), because the two are kept
    /// for different reasons: a checkout is evidence, at 14 MB, and its build
    /// output is a cache, at 20–36 GB. Governed by one window they were kept for
    /// the same week, and a week of Rust checkouts does not fit on a 150 GB disk
    /// — the box that prompted this hit 76% with eight of them, 99.96% of it
    /// regenerable.
    ///
    /// This clock starts when the *attempt* ends, and does not wait for the task
    /// to leave the board or for a pull request to close: nothing reads `target/`
    /// after the run does. `on-settle` (the default) is no window at all. The one
    /// guard is that nobody is building in there — see
    /// [`crate::gc::cache_standing`]. `off` keeps every cache for as long as its
    /// checkout lives.
    #[serde(default = "default_retain_build_output")]
    pub retain_build_output: String,
    /// How long a board-dispatched chat is kept on its space's shelf before the
    /// board archives it (gh#139). The same clock
    /// [`retain_worktrees`](Self::retain_worktrees) runs on — it starts when
    /// the attempt is closed *and* its task has left the board — and for the
    /// same reason: a chat whose task is still owed something is a chat
    /// somebody is still using. Overridden per route.
    ///
    /// **`on-settle`** — no window at all. gh#139 gave the chat the checkout's
    /// week on the theory that they are one attempt's leavings, but they are
    /// not read the same way: a checkout is evidence you might go back for, a
    /// chat is a row you are *shown*, and a merged task's row has nothing left
    /// to say. Every guard is in [`crate::gc::chat_standing`] and none of them
    /// is the clock: a live attempt, a blocked one, an open pull request, an
    /// issue still open, or work the chat released that has not left the board
    /// (gh#354) all hold the chat on the shelf regardless. What is left
    /// when they let go is a task that merged or closed, and a row for it is
    /// the landfill gh#139 was about. A duration still works — `2d`, `1w` — for
    /// a space that wants a grace period. `off` (or
    /// `0`) keeps every chat on the shelf forever, which is what every board
    /// did before this existed — and what the operator question behind gh#139
    /// ("do all complete sessions just accumulate under the folder?") was
    /// about. Archiving is not deleting: the Archived page unarchives, and the
    /// transcript is untouched either way.
    #[serde(default = "default_archive_chats")]
    pub archive_chats: String,
    /// What the board does about a dispatch that spends somebody else's
    /// subscription (gh#101): `warn` (the default), `require-own`, or `off`.
    /// Overridden per route.
    ///
    /// `warn` says so everywhere and releases anyway — in the pickers, on the
    /// CLI, in the upstream comment and on the row for the attempt's whole
    /// life. `require-own` refuses instead, unless the dispatch names the payer
    /// outright (`--bill`). `off` says nothing, which is the right answer on a
    /// box where one person's plan pays for everything.
    ///
    /// What the match is made of depends on where the dispatch came from
    /// (gh#161): a relayed one is compared against the identity the edge
    /// verified and the relay stamped on the frame, which no frontend can
    /// write; one issued on the box carries no stamp and is compared against
    /// the frontend's `viaUser`, which is all a local shell can be asked for.
    /// See [`crate::billing`].
    #[serde(default = "default_billing_guard")]
    pub billing_guard: String,
    /// Whether a dispatch writes the board's conventions into the instruction
    /// file its runtime reads — `CLAUDE.md` in the Claude config dir,
    /// `AGENTS.md` in `CODEX_HOME` (gh#272). On by default. Overridden per
    /// route.
    ///
    /// On, because the alternative is what the board did before: a Claude
    /// dispatch found the skill in the config dir the engine pointed it at, and
    /// a Codex one — which has no skill mechanism — was the one agent on the box
    /// that had never heard of the board that started it. Nothing about the
    /// contract is optional in the way a *setting* usually is; the flag exists
    /// for the two boxes where the write is unwelcome rather than for the
    /// choice being interesting.
    ///
    /// Which is: a box whose operator keeps their own `~/.claude/CLAUDE.md` and
    /// wants nothing written into it (a dispatch naming no account reads
    /// exactly that file, so that is where the block lands), and an account slot
    /// shared with work that is not the board's. Turning it off does not merely
    /// stop writing — the next dispatch on that route takes the block back out,
    /// because slot dirs are reused and a stale contract is worse than none.
    /// Everything outside the markers is left alone in both directions; see
    /// [`crate::conventions`].
    #[serde(default = "default_true")]
    pub agent_instructions: bool,
    /// MCP stdio servers every dispatched chat receives (gh#273).
    ///
    /// The shipped default is the board itself: agents can inspect their task,
    /// related attempts, and release work through typed tools instead of
    /// shelling out. A route can replace the whole list — including with `[]`
    /// to opt out — because route-specific tools must not bleed into another
    /// route sharing the same account slot.
    #[serde(default = "default_mcp_servers")]
    pub mcp_servers: Vec<comet_proto::McpServer>,
    /// Per-model rate overrides for the spend figures (gh#182), in US dollars
    /// per million tokens:
    ///
    /// ```toml
    /// [defaults.rates."claude-opus-5"]
    /// input = 5.0
    /// output = 25.0
    /// # cache_read and cache_write are derived from `input` at the published
    /// # multipliers (0.1x and 1.25x) unless written out here.
    /// ```
    ///
    /// The board ships a dated table of published list prices
    /// ([`comet_proto::view::rates::builtin`]) and this overrides it, per
    /// model. Two things need that: a rate the table is missing or wrong about
    /// — the table is a snapshot and says so — and a negotiated rate, which no
    /// lookup anywhere would ever know.
    ///
    /// Keys are model ids as the *harness* reports them, matched the way the
    /// table matches: exactly, else by the longest family prefix, so one entry
    /// covers every dated snapshot of a model.
    ///
    /// **Last resort, not first.** A model with no rate is reported unpriced
    /// rather than free, so an empty table here is an honest board and not a
    /// broken one.
    #[serde(default)]
    pub rates: BTreeMap<String, RateOverride>,
}

/// One model's rate, as `routing.toml` writes it: dollars per million tokens.
///
/// The two cache rates are optional because they are *derivable* — a cache read
/// is a tenth of fresh input and a five-minute write a quarter more than it,
/// which is how the shipped table computes its own rows. Writing all four is
/// for the case where a provider prices them differently; writing two is the
/// common case, and it cannot be got wrong by fat-fingering a multiplier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateOverride {
    /// Fresh input, $/million tokens.
    pub input: comet_proto::view::rates::Usd,
    /// Output, $/million tokens.
    pub output: comet_proto::view::rates::Usd,
    /// Cached input. Omitted = 0.1× `input`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<comet_proto::view::rates::Usd>,
    /// Cache writes. Omitted = 1.25× `input`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<comet_proto::view::rates::Usd>,
}

impl RateOverride {
    /// This override as a rate, with the cache halves derived where they were
    /// left out.
    pub fn rate(&self) -> comet_proto::view::rates::ModelRate {
        let derived = comet_proto::view::rates::ModelRate::published(
            self.input.dollars(),
            self.output.dollars(),
        );
        comet_proto::view::rates::ModelRate {
            input: self.input,
            output: self.output,
            cache_read: self.cache_read.unwrap_or(derived.cache_read),
            cache_write: self.cache_write.unwrap_or(derived.cache_write),
        }
    }

    /// What is wrong with this rate, said the way `validate` says everything
    /// else: a negative price is a typo, and a typo that priced a window at
    /// minus four dollars would be discovered by somebody reading the page.
    fn check(&self) -> Result<(), String> {
        for (field, amount) in [
            ("input", self.input),
            ("output", self.output),
            ("cache_read", self.cache_read.unwrap_or_default()),
            ("cache_write", self.cache_write.unwrap_or_default()),
        ] {
            if amount.micros < 0 {
                return Err(format!(
                    "`{field}` is {}, and a rate cannot be negative; write it as dollars per \
                     million tokens, like `{field} = 5.0`",
                    amount.dollars()
                ));
            }
        }
        Ok(())
    }
}

fn default_billing_guard() -> String {
    crate::billing::GuardMode::default().as_str().to_string()
}

impl Defaults {
    /// The pinned orchestrator's chat id, if there is one.
    ///
    /// Trimmed, and an empty value is `None`. A settings surface that clears a
    /// text field writes `orchestrator_chat = ""` at least as often as it
    /// removes the key, and a board that then tried to prompt a chat named ""
    /// would log a delivery failure every settle for as long as nobody noticed.
    pub fn orchestrator(&self) -> Option<&str> {
        self.orchestrator_chat
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
    }
}

fn default_max_duration() -> String {
    "2h".into()
}

fn default_max_tool_failures() -> String {
    "10".into()
}

fn default_max_tool_calls() -> String {
    "2000".into()
}

fn default_retain_worktrees() -> String {
    "7d".into()
}

fn default_archive_chats() -> String {
    "on-settle".into()
}

fn default_retain_build_output() -> String {
    "on-settle".into()
}

fn default_new_source() -> String {
    "github".into()
}

fn default_max_concurrent() -> usize {
    3
}

fn default_memory_headroom() -> String {
    format!("{:.0}%", crate::pressure::DEFAULT_HEADROOM * 100.0)
}

/// Impl spec §5. The design fixtures show `lin-145-altinn-retry`, and gh#364
/// has now arrived at that from the other end — `{identifier_lower}` carries
/// the identifier and a slug of the title, so the default renders
/// `board/lin-145-altinn-retry-fails` rather than the identifier alone. The
/// template is config either way; what it interpolates to is
/// [`crate::dispatch::branch_slug`].
fn default_branch_template() -> String {
    "board/{identifier_lower}".into()
}

/// The remote's default branch — the base a person means when they say "cut it
/// from main" without naming which main.
fn default_base() -> String {
    "origin/HEAD".into()
}

fn default_mcp_servers() -> Vec<comet_proto::McpServer> {
    vec![comet_proto::McpServer {
        name: "comet-board".into(),
        command: "comet-board".into(),
        args: vec!["mcp".into()],
    }]
}

impl Default for Defaults {
    fn default() -> Self {
        Defaults {
            max_concurrent_per_workspace: default_max_concurrent(),
            min_memory_headroom: default_memory_headroom(),
            branch_template: default_branch_template(),
            base: default_base(),
            notify: true,
            notify_webhook: None,
            notify_dispatcher: true,
            orchestrator_chat: None,
            new_source: default_new_source(),
            max_duration: default_max_duration(),
            max_tool_failures: default_max_tool_failures(),
            max_tool_calls: default_max_tool_calls(),
            retain_worktrees: default_retain_worktrees(),
            retain_build_output: default_retain_build_output(),
            archive_chats: default_archive_chats(),
            billing_guard: default_billing_guard(),
            agent_instructions: true,
            mcp_servers: default_mcp_servers(),
            rates: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Leave a trail on GitHub: a comment on dispatch and on outcome, and
    /// close the issue when the task is done.
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Per-route override of `defaults.max_tool_failures` (gh#270) — how many
    /// tool calls may fail in a row before this route's runs are steered and
    /// then stopped, e.g. `"20"`, or `"off"` for none.
    ///
    /// Per route for [`Route::max_duration`]'s reason, and one more: what
    /// counts as a normal number of failures is a property of the *work*. A
    /// route pointed at a repo whose test suite is flaky under load fails a lot
    /// without being stuck, and a route that edits config files does not fail
    /// at all — one number for both has to be set by the noisier one, which
    /// leaves the quiet route unguarded.
    #[serde(default)]
    pub max_tool_failures: Option<String>,
    /// Per-route override of `defaults.max_tool_calls` (gh#270) — how many tool
    /// calls one turn on this route may make, e.g. `"5000"`, or `"off"`.
    #[serde(default)]
    pub max_tool_calls: Option<String>,
    /// Per-route override of `defaults.archive_chats` (gh#139) — how long this
    /// route's finished chats stay on their space's shelf, e.g. `"30d"`, or
    /// `"off"` to keep them there.
    ///
    /// Per route because a shelf belongs to a space, and routes are how work is
    /// pointed at spaces: the route that runs a hundred throwaway fixes a week
    /// into a scratch space is not the route into the repo whose finished chats
    /// somebody actually re-reads, and a single window has to be set by
    /// whichever of them you would most regret losing.
    #[serde(default)]
    pub archive_chats: Option<String>,
    /// Per-route override of `defaults.billing_guard` (gh#101).
    ///
    /// Per route because the answer is a property of the work, not of the box:
    /// the route into a shared team repo is the one where a teammate spending
    /// the owner's plan is worth refusing, and the route into the owner's own
    /// side project is the one where the warning is noise on every dispatch.
    #[serde(default)]
    pub billing_guard: Option<String>,
    /// Per-route override of `defaults.agent_instructions` (gh#272) — whether
    /// this route's dispatches write the board's conventions into the
    /// instruction file their runtime reads.
    ///
    /// Per route because the file belongs to an *account*, and routes are how
    /// work is pointed at accounts: the route running the board's own repos
    /// under a slot nothing else touches is not the route pointed at a
    /// teammate's login that they also sit at. The second is the one worth
    /// being able to answer separately.
    #[serde(default)]
    pub agent_instructions: Option<bool>,
    /// Replace `[defaults].mcp_servers` for this route (gh#273). `None`
    /// inherits; an explicit empty list disables MCP injection.
    #[serde(default)]
    pub mcp_servers: Option<Vec<comet_proto::McpServer>>,
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

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RouteMatch {
    /// Legacy matchers from the Linear era (gh#471). Still parsed so an old
    /// routing.toml keeps loading, but no task carries a team or project any
    /// more, so a route that specifies either matches **nothing** — it goes
    /// inert rather than becoming a catch-all or a parse error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linear_team: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linear_project: Option<String>,
    pub gh_repo: Option<String>,
    pub label: Option<String>,
}

impl RouteMatch {
    /// A route with no `match` matches everything, so it must come last —
    /// [`RoutingConfig::validate`] refuses a config where one does not, and the
    /// adoption writer inserts ahead of it rather than after. A legacy
    /// `linear_team`/`linear_project` matcher counts as a matcher here — it
    /// matches nothing, which is the opposite of matching everything.
    pub fn is_empty(&self) -> bool {
        self.linear_team.is_none()
            && self.linear_project.is_none()
            && self.gh_repo.is_none()
            && self.label.is_none()
    }
}

/// One auto-pick rule (gh#490): which ready tasks it matches, what a dispatch
/// under it runs as, how much it may do at once, and whose automation it is.
///
/// Deliberately deterministic — a label match, never a model call. The board's
/// existing policies (one live attempt, routes, capacity, billing guard,
/// credentials) stay authoritative at dispatch; the rule only decides *that*
/// a dispatch is asked for, and records why when it is not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Automation {
    /// The rule's name — how its dispatches are attributed, how its history is
    /// keyed, and how the settings page addresses it. Required and unique.
    pub name: String,
    /// Off is paused: the rule is kept, its history is kept, and nothing new
    /// is dispatched. Off by default, because enabling is the authorization.
    #[serde(default)]
    pub enabled: bool,
    /// The human responsible for this automation, named on every attempt it
    /// releases. Required to *enable* — an automation nobody answers for is
    /// the thing this field exists to prevent — but a rule being drafted may
    /// not have one yet.
    #[serde(default)]
    pub owner: Option<String>,
    /// Match only tasks from this source (`github`). Unset matches any.
    #[serde(default)]
    pub source: Option<String>,
    /// Labels a task must carry — all of them. At least one is required to
    /// enable: a rule with no required label would match every ready task on
    /// the board, and "dispatch everything" is a decision nobody makes by
    /// leaving a list empty.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Labels that exclude a task even when the required ones match.
    #[serde(default)]
    pub exclude_labels: Vec<String>,
    /// Only dispatch tasks whose resolved route is this one (by display name).
    /// Unset takes whatever route the task resolves to — the rule never
    /// invents a route of its own, so an unrouted task is skipped either way.
    #[serde(default)]
    pub route: Option<String>,
    /// Runtime override for the rule's dispatches. Unset runs the route's.
    #[serde(default)]
    pub runtime: Option<String>,
    /// Model override for the rule's dispatches. Unset runs the harness default.
    #[serde(default)]
    pub model: Option<String>,
    /// The agent-account slot the rule's dispatches spend — **explicit**, and
    /// consent: an unattended dispatch has nobody at a confirm dialog, so the
    /// account is acknowledged here, once, when a human writes it down. A rule
    /// without one refuses its candidates with `billing account missing`
    /// rather than silently spending the box's own login.
    #[serde(default)]
    pub account: Option<String>,
    /// Most dispatches one evaluation may make. Evaluations run per sync cycle
    /// and per eligibility change, so this is a rate more than a ceiling.
    #[serde(default = "default_automation_per_eval")]
    pub max_per_eval: usize,
    /// Most live attempts this rule may have running at once — its own cap,
    /// inside the workspace capacity that still applies on top.
    #[serde(default = "default_automation_concurrent")]
    pub max_concurrent: usize,
    /// Most dispatches per rolling 24 hours. Unset is unbounded — the
    /// concurrency cap above still holds.
    #[serde(default)]
    pub daily_budget: Option<usize>,
    /// How long a task waits after this rule's attempt on it failed, or after
    /// a dispatch was refused, before the rule considers it again. What keeps
    /// a refusal from hot-looping: the reason is recorded and the task is
    /// deferred, never retried inside the window.
    #[serde(default = "default_automation_cooldown")]
    pub cooldown: String,
}

fn default_automation_per_eval() -> usize {
    1
}

fn default_automation_concurrent() -> usize {
    1
}

fn default_automation_cooldown() -> String {
    "30m".into()
}

impl Automation {
    /// The failure/refusal cooldown in seconds. `None` is "off" — reconsider
    /// on the next evaluation. An unparseable value has been refused by
    /// validation; a config that got here with one reads as the default,
    /// never as "no cooldown" — the wrong way to fail is the one that
    /// hot-loops a refusal.
    pub fn cooldown_secs(&self) -> Option<u64> {
        parse_retention(&self.cooldown)
            .unwrap_or_else(|_| parse_retention(&default_automation_cooldown()).ok().flatten())
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
            // And here a typo reads as "unbounded" (gh#270) — a route somebody
            // deliberately tightened, silently running without a guardrail.
            if let Some(f) = &r.max_tool_failures
                && let Err(e) = parse_turn_limit(f, crate::spin::MIN_TOOL_FAILURES)
            {
                out.push(format!(
                    "route {} ({}) has max_tool_failures {e}",
                    i + 1,
                    r.display_name()
                ));
            }
            if let Some(c) = &r.max_tool_calls
                && let Err(e) = parse_turn_limit(c, crate::spin::MIN_TOOL_CALLS)
            {
                out.push(format!(
                    "route {} ({}) has max_tool_calls {e}",
                    i + 1,
                    r.display_name()
                ));
            }
            // A typo here reads as "keep every chat forever" (gh#139), which
            // is the shelf silting up again behind a key somebody did set.
            if let Some(a) = &r.archive_chats
                && let Err(e) = parse_chat_retention(a)
            {
                out.push(format!(
                    "route {} ({}) has archive_chats {e}",
                    i + 1,
                    r.display_name()
                ));
            }
            // And here a typo reads as `warn` — the default — which is the one
            // spelling that would silently un-arm a route somebody deliberately
            // set to `require-own` (gh#101).
            if let Some(g) = &r.billing_guard
                && let Err(e) = crate::billing::parse_guard_mode(g)
            {
                out.push(format!(
                    "route {} ({}) has billing_guard {e}",
                    i + 1,
                    r.display_name()
                ));
            }
            if let Some(servers) = &r.mcp_servers {
                mcp_server_problems(
                    &format!("route {} ({})", i + 1, r.display_name()),
                    servers,
                    &mut out,
                );
            }
        }
        mcp_server_problems("[defaults]", &self.defaults.mcp_servers, &mut out);
        if let Err(e) = parse_max_duration(&self.defaults.max_duration) {
            out.push(format!("[defaults] max_duration {e}"));
        }
        if let Err(e) = crate::billing::parse_guard_mode(&self.defaults.billing_guard) {
            out.push(format!("[defaults] billing_guard {e}"));
        }
        // Same reasoning as the cap above, in both directions: a typo that read
        // as `off` would leave the box unguarded on the night it matters, and
        // one that read as a floor no box clears would stop the board dead.
        if let Err(e) = parse_memory_headroom(&self.defaults.min_memory_headroom) {
            out.push(format!("[defaults] min_memory_headroom {e}"));
        }
        if let Err(e) = parse_turn_limit(
            &self.defaults.max_tool_failures,
            crate::spin::MIN_TOOL_FAILURES,
        ) {
            out.push(format!("[defaults] max_tool_failures {e}"));
        }
        if let Err(e) = parse_turn_limit(&self.defaults.max_tool_calls, crate::spin::MIN_TOOL_CALLS)
        {
            out.push(format!("[defaults] max_tool_calls {e}"));
        }
        // Same reasoning as the cap above: an unparseable retention would read
        // as `off` on a board nobody told, and the checkouts would pile up
        // exactly as they did before gh#72.
        if let Err(e) = parse_retention(&self.defaults.retain_worktrees) {
            out.push(format!("[defaults] retain_worktrees {e}"));
        }
        if let Err(e) = parse_chat_retention(&self.defaults.archive_chats) {
            out.push(format!("[defaults] archive_chats {e}"));
        }
        if let Err(e) = parse_build_retention(&self.defaults.retain_build_output) {
            out.push(format!("[defaults] retain_build_output {e}"));
        }
        // A rate is money on a page somebody makes decisions on (gh#182), and a
        // negative or misspelled one would be *believed* — priced totals have
        // no plausibility check of their own.
        for (model, rate) in &self.defaults.rates {
            if model.trim().is_empty() {
                out.push(
                    "[defaults.rates] has an entry with an empty model name; the key is the \
                     model id a run reports, like `[defaults.rates.\"claude-opus-5\"]`"
                        .to_string(),
                );
            }
            if let Err(e) = rate.check() {
                out.push(format!("[defaults.rates.\"{model}\"] {e}"));
            }
        }
        // And the same for the other half: a plan cost is what the subsidy
        // figure is divided by.
        for (slot, account) in &self.accounts {
            if account.monthly_usd.micros < 0 {
                out.push(format!(
                    "[account.\"{slot}\"] monthly_usd is {}, and a subscription cannot cost \
                     less than nothing; write what the plan costs per month, like \
                     `monthly_usd = 200`",
                    account.monthly_usd.dollars()
                ));
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
        // Auto-pick rules (gh#490). A *disabled* rule may be half-written —
        // that is what the settings page creates and fills in — so the
        // enable-gated requirements (owner and labels; the account is refused
        // at the write seam in `routes.rs` and again at dispatch, never here,
        // so a hand-written file that predates gh#524 still loads) only bite
        // on `enabled = true`. What is checked on every
        // rule is what would misbehave silently: a duplicate name would make
        // one history out of two rules, and an unparseable cooldown would
        // read as the default while somebody believes they set it.
        for (i, a) in self.automations.iter().enumerate() {
            let name = a.name.trim();
            if name.is_empty() {
                out.push(format!(
                    "automation {} has no name; the name is how its dispatches \
                     are attributed and its history is kept",
                    i + 1
                ));
            }
            if self.automations[..i]
                .iter()
                .any(|earlier| earlier.name.trim().eq_ignore_ascii_case(name) && !name.is_empty())
            {
                out.push(format!(
                    "automation `{name}` appears twice; two rules with one name \
                     would share one history and one concurrency cap"
                ));
            }
            if let Some(source) = &a.source
                && crate::model::Source::parse(source).is_none()
            {
                out.push(format!(
                    "automation `{name}` matches source `{source}`, which is not \
                     a board source. Write `github`, or drop the key."
                ));
            }
            if let Some(runtime) = &a.runtime
                && harness_for_runtime(runtime).is_none()
            {
                out.push(format!(
                    "automation `{name}` has runtime `{runtime}`, which is not a \
                     comet harness. Known runtimes: {}",
                    RUNTIME_NAMES.join(", ")
                ));
            }
            if let Err(e) = parse_retention(&a.cooldown) {
                out.push(format!("automation `{name}` has cooldown {e}"));
            }
            if a.enabled {
                if a.owner.as_deref().map(str::trim).unwrap_or("").is_empty() {
                    out.push(format!(
                        "automation `{name}` is enabled with no owner; an \
                         automation that dispatches agents must name the human \
                         responsible for it"
                    ));
                }
                if a.labels.iter().all(|l| l.trim().is_empty()) {
                    out.push(format!(
                        "automation `{name}` is enabled with no required labels; \
                         it would match every ready task on the board. Name the \
                         label that means \"approved for autonomous execution\"."
                    ));
                }
            }
        }
        // A `[users]` value that is not an address is worse than no entry: the
        // engine would stamp it onto `GIT_AUTHOR_EMAIL` and every commit that
        // teammate's dispatches produce would be unattributable — which is the
        // exact failure the table exists to remove (gh#107).
        for (who, author) in &self.users {
            if crate::git_identity::parse_author(author).is_none() {
                out.push(format!(
                    "[users] \"{who}\" = \"{author}\" is not a git author — write an \
                     email address, or `Name <email>`. GitHub attributes commits to \
                     the account owning the address; \
                     `<id>+<login>@users.noreply.github.com` from \
                     https://github.com/settings/emails always works."
                ));
            }
        }
        out
    }

    /// The git author for whoever released a dispatch (gh#107), or `None` when
    /// this board has no address for them — in which case the box's own
    /// identity authors, exactly as it did before the map existed.
    ///
    /// Case-insensitive on the key: the value arrives from whichever frontend
    /// the teammate signed in on, and an email is not case-sensitive in the
    /// half that matters. An unparseable value is `None` rather than a guess;
    /// [`RoutingConfig::problems`] reports it as the config error it is.
    pub fn git_author_for(&self, user: &str) -> Option<comet_proto::GitAuthor> {
        let user = user.trim();
        if user.is_empty() {
            return None;
        }
        self.users
            .iter()
            .find(|(k, _)| k.trim().eq_ignore_ascii_case(user))
            .and_then(|(_, v)| crate::git_identity::parse_author(v))
    }

    /// Which GitHub account the map says somebody is (gh#369) — the login
    /// inside the address [`RoutingConfig::git_author_for`] resolves.
    ///
    /// Only a noreply address answers. GitHub minted it and no other account
    /// can hold it, so the login inside it is a fact rather than a guess; a
    /// work address on the same account is a perfectly good *commit* author and
    /// still says nothing about which login it belongs to. That asymmetry is
    /// deliberate: a commit author is a claim anybody may write, and this
    /// chooses which credential a review is cast under.
    pub fn github_login_for(&self, user: &str) -> Option<String> {
        let author = self.git_author_for(user)?;
        crate::git_identity::noreply_login(&author.email).map(str::to_string)
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

    /// The share of the box's memory that must still be available before a
    /// dispatch may start (gh#533). `None` is the gate off.
    ///
    /// Board-wide and not per route, unlike every cap beside it: memory is the
    /// one resource routes cannot have their own budget of, because they all
    /// spend the same box. A per-route floor would only mean whichever route
    /// dispatched next got to decide how full the box may be.
    ///
    /// Validation has already refused an unparseable value; a config that
    /// reached here with one is one `load_or_default` fell back on, so the
    /// default floor is the honest answer rather than no gate at all — the same
    /// rule [`Self::max_duration_secs`] follows, and for the same reason.
    pub fn min_memory_headroom(&self) -> Option<f64> {
        parse_memory_headroom(&self.defaults.min_memory_headroom).unwrap_or_else(|_| {
            parse_memory_headroom(&default_memory_headroom())
                .ok()
                .flatten()
        })
    }

    /// What the box says about starting one more agent right now (gh#533), or
    /// [`crate::pressure::Headroom::Unknown`] when the gate is off.
    ///
    /// One call rather than two so the reading and the floor it is judged
    /// against cannot be taken from different configs — and so `off` never
    /// reaches `/proc` at all.
    pub fn headroom(&self) -> crate::pressure::Headroom {
        match self.min_memory_headroom() {
            Some(floor) => crate::pressure::headroom(&crate::pressure::Snapshot::read(), floor),
            None => crate::pressure::Headroom::Unknown,
        }
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

    /// The turn-level guardrails for attempts on a route (gh#270) — the
    /// route's own `max_tool_failures` / `max_tool_calls`, else the
    /// `[defaults]` pair. Either half `None` is that half unbounded.
    ///
    /// `route` is an `Option` for [`Self::max_duration_secs`]'s reason: no
    /// route means the board-wide answer rather than none at all, so deleting
    /// a route is not the way to escape the guard. Unlike the duration cap
    /// these are resolved once, at dispatch, and ride the chat — the engine
    /// enforces them inside the run loop, where there is no board to ask.
    pub fn turn_limits(&self, route: Option<&Route>) -> comet_proto::TurnLimits {
        let raw =
            |own: Option<&str>, fallback: &str| -> String { own.unwrap_or(fallback).to_string() };
        let failures = raw(
            route.and_then(|r| r.max_tool_failures.as_deref()),
            &self.defaults.max_tool_failures,
        );
        let calls = raw(
            route.and_then(|r| r.max_tool_calls.as_deref()),
            &self.defaults.max_tool_calls,
        );
        // Validation has already refused an unparseable value; a config that
        // reached here with one is one `load_or_default` fell back on, so the
        // shipped default is the honest answer — never "unbounded", which is
        // the one wrong way to fail.
        comet_proto::TurnLimits {
            tool_failures: parse_turn_limit(&failures, crate::spin::MIN_TOOL_FAILURES)
                .unwrap_or_else(|_| {
                    parse_turn_limit(&default_max_tool_failures(), crate::spin::MIN_TOOL_FAILURES)
                        .ok()
                        .flatten()
                }),
            tool_calls: parse_turn_limit(&calls, crate::spin::MIN_TOOL_CALLS).unwrap_or_else(
                |_| {
                    parse_turn_limit(&default_max_tool_calls(), crate::spin::MIN_TOOL_CALLS)
                        .ok()
                        .flatten()
                },
            ),
        }
    }

    /// What this route does about a cross-billed dispatch — the route's own
    /// `billing_guard`, else `defaults.billing_guard` (gh#101).
    ///
    /// `route` is an `Option` for the same reason [`Self::max_duration_secs`]'s
    /// is: the guard is also read outside dispatch (`doctor`, and the row
    /// detail on an attempt whose route was renamed under it), and no route
    /// means the board-wide answer. An unparseable value has already been
    /// refused by validation; a config that reached here with one is one
    /// `load_or_default` fell back on, so it reads as the default mode — never
    /// as `off`, which is the one wrong way to fail.
    pub fn billing_guard(&self, route: Option<&Route>) -> crate::billing::GuardMode {
        let raw = route
            .and_then(|r| r.billing_guard.as_deref())
            .unwrap_or(&self.defaults.billing_guard);
        crate::billing::parse_guard_mode(raw).unwrap_or_default()
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

    /// How long the build output inside a finished attempt's checkout is kept,
    /// in seconds. `Some(0)` is `on-settle` — swept as the attempt ends — and
    /// `None` is "for as long as the checkout lives": sweeping off (gh#186).
    ///
    /// Board-wide for [`Self::retain_worktrees_secs`]'s reason, and more so: what
    /// a cache costs is disk, the disk is the box's and not the route's, and a
    /// per-route window would be one more place for 36 GB to hide behind a key
    /// nobody set. An unparseable value falls back to the default window, never
    /// to "keep forever" — the wrong way to fail here is the one that fills the
    /// disk silently.
    pub fn retain_build_output_secs(&self) -> Option<u64> {
        parse_build_retention(&self.defaults.retain_build_output).unwrap_or_else(|_| {
            parse_build_retention(&default_retain_build_output())
                .ok()
                .flatten()
        })
    }

    /// How long a finished attempt's chat stays on its space's shelf, in
    /// seconds — the route's own `archive_chats`, else
    /// `defaults.archive_chats`. `None` is "forever": archiving off (gh#139).
    ///
    /// Per route, unlike [`Self::retain_worktrees_secs`], because a chat lives
    /// on a *space's* shelf and a route is what points work at a space — see
    /// [`Route::archive_chats`]. `route` is an `Option` for the same reason
    /// [`Self::max_duration_secs`]'s is: the sweep runs long after dispatch,
    /// over attempts whose route may since have been renamed or deleted, and no
    /// route means the board-wide window rather than "never". An unparseable
    /// value falls back to the default window, never to "forever".
    pub fn archive_chats_secs(&self, route: Option<&Route>) -> Option<u64> {
        let raw = route
            .and_then(|r| r.archive_chats.as_deref())
            .unwrap_or(&self.defaults.archive_chats);
        parse_chat_retention(raw).unwrap_or_else(|_| {
            parse_chat_retention(&default_archive_chats())
                .ok()
                .flatten()
        })
    }

    /// Does a dispatch on this route write the board's conventions into its
    /// runtime's instruction file (gh#272)? The route's own answer, else
    /// `defaults.agent_instructions`, which is on.
    ///
    /// Resolved at dispatch and carried on the spec, exactly as
    /// [`Self::turn_limits`] is and for the same reason: the write happens
    /// inside the engine, beside the config dir it just materialized, and that
    /// code has no board to ask.
    pub fn agent_instructions(&self, route: Option<&Route>) -> bool {
        route
            .and_then(|r| r.agent_instructions)
            .unwrap_or(self.defaults.agent_instructions)
    }

    /// MCP servers for a dispatch on `route` (gh#273): the route's whole list
    /// when it has one, else `[defaults]`. Returning the stored slice keeps the
    /// routing interface small; the dispatch spec takes the one owned copy
    /// that crosses into the engine.
    pub fn mcp_servers<'a>(&'a self, route: Option<&'a Route>) -> &'a [comet_proto::McpServer] {
        route
            .and_then(|r| r.mcp_servers.as_deref())
            .unwrap_or(&self.defaults.mcp_servers)
    }
}

/// MCP names have to survive three configuration syntaxes and three tool-name
/// normalizers. Keep the common spelling deliberately small, and reject names
/// that collapse onto the same prefix (`foo-bar` / `foo_bar`) before a harness
/// gets to pick one silently.
fn mcp_server_problems(scope: &str, servers: &[comet_proto::McpServer], out: &mut Vec<String>) {
    let mut names = std::collections::BTreeSet::new();
    for (i, server) in servers.iter().enumerate() {
        let name = server.name.trim();
        if name.is_empty()
            || name != server.name
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            out.push(format!(
                "{scope} MCP server {} has name `{}`; use only letters, numbers, `-`, and `_`, with no surrounding whitespace",
                i + 1,
                server.name
            ));
        }
        let normalized = name
            .bytes()
            .map(|b| match b {
                b'-' | b'_' => b'_',
                _ => b.to_ascii_lowercase(),
            })
            .collect::<Vec<_>>();
        if !name.is_empty() && !names.insert(normalized) {
            out.push(format!(
                "{scope} MCP server name `{}` collides with an earlier name after tool-name normalization",
                server.name
            ));
        }
        if server.command.trim().is_empty() || server.command.trim() != server.command {
            out.push(format!(
                "{scope} MCP server `{}` has an empty command or surrounding whitespace",
                server.name
            ));
        }
    }
}

/// All *specified* keys must match (AND). Unspecified keys are ignored.
fn route_matches(m: &RouteMatch, ctx: &RouteContext) -> bool {
    // Legacy Linear matchers (gh#471): no task carries a team or project any
    // more, so a route still keyed on one matches nothing at all.
    if m.linear_team.is_some() || m.linear_project.is_some() {
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
    use comet_proto::view::rates::Usd;

    const SAMPLE: &str = r#"
[sync]
interval = "30s"

[[route]]
match = { gh_repo = "florin-as/offhand" }
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
        assert_eq!(c.defaults.max_concurrent_per_workspace, 3);
    }

    #[test]
    fn first_matching_route_wins() {
        let c = cfg();
        // Matches both the repo route and the label route; the repo route is
        // declared first.
        let ctx = RouteContext {
            gh_repo: Some("florin-as/offhand".into()),
            labels: vec!["fintech".into()],
        };
        assert_eq!(c.resolve(&ctx).unwrap().workspace, "offhand");
    }

    #[test]
    fn label_route_matches_when_repo_does_not() {
        let c = cfg();
        let ctx = RouteContext {
            gh_repo: Some("florin-as/tally".into()),
            labels: vec!["fintech".into()],
        };
        assert_eq!(c.resolve(&ctx).unwrap().workspace, "fintech");
    }

    #[test]
    fn unmatched_task_has_no_route() {
        let c = cfg();
        let ctx = RouteContext {
            gh_repo: Some("florin-as/tally".into()),
            labels: vec!["chore".into()],
        };
        assert!(c.resolve(&ctx).is_none());
    }

    #[test]
    fn match_keys_are_anded() {
        let c: RoutingConfig = toml::from_str(
            r#"
[[route]]
match = { gh_repo = "o/r", label = "herd" }
workspace = "w"
repo = "/tmp"
runtime = "claude"
"#,
        )
        .unwrap();
        let repo_only = RouteContext {
            gh_repo: Some("o/r".into()),
            ..Default::default()
        };
        assert!(c.resolve(&repo_only).is_none());
        let both = RouteContext {
            gh_repo: Some("o/r".into()),
            labels: vec!["herd".into()],
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

    /// gh#471: Linear-era keys still on disk — the `[linear]` table, a route's
    /// `linear_team` matcher, `[sync] labels` — must not stop an existing
    /// board from loading its config. The tables parse as unknown keys; a
    /// route still matching on a team stays a valid route that matches
    /// nothing, never a catch-all and never a validation failure.
    #[test]
    fn leftover_linear_keys_load_and_route_nothing() {
        let text = r#"
[sync]
labels = ["herd"]

[linear]
review_state = "In Review"

[[route]]
match = { linear_team = "OFF" }
workspace = "w"
repo = "/tmp"
runtime = "claude"

[[route]]
match = { gh_repo = "o/r" }
workspace = "w2"
repo = "/tmp"
runtime = "claude"
"#;
        let c: RoutingConfig = toml::from_str(text).unwrap();
        c.validate()
            .expect("a legacy linear route is not a route with no match");
        let ctx = RouteContext {
            gh_repo: Some("o/r".into()),
            labels: vec!["herd".into()],
        };
        assert_eq!(
            c.resolve(&ctx).unwrap().workspace,
            "w2",
            "the legacy route matches nothing; the GitHub route still wins"
        );
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

    // ---- the billing guard (gh#101) --------------------------------------

    #[test]
    fn the_billing_guard_warns_unless_a_route_says_otherwise() {
        use crate::billing::GuardMode;
        // Warn is the shipped answer: the board's job is to make the spend
        // visible, and a box where two people share one plan is a normal box.
        assert_eq!(
            RoutingConfig::default().billing_guard(None),
            GuardMode::Warn
        );

        let c = github(
            r#"
[defaults]
billing_guard = "off"

[[route]]
match = { label = "team" }
workspace = "w"
repo = "/tmp"
runtime = "claude"
billing_guard = "require-own"

[[route]]
match = { label = "mine" }
workspace = "w"
repo = "/tmp"
runtime = "claude"
"#,
        );
        assert_eq!(c.billing_guard(Some(&c.routes[0])), GuardMode::RequireOwn);
        assert_eq!(c.billing_guard(Some(&c.routes[1])), GuardMode::Off);
        // A route deleted from under a running attempt falls back to the
        // board's answer, whatever that is.
        assert_eq!(c.billing_guard(None), GuardMode::Off);
    }

    /// A typo reads exactly like the default, which is the one wrong way to
    /// fail: it would silently un-arm a route somebody set to `require-own`.
    #[test]
    fn an_unknown_billing_guard_is_refused_by_name() {
        let c: RoutingConfig =
            toml::from_str("[defaults]\nbilling_guard = \"require-mine\"\n").unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("[defaults] billing_guard"), "{err}");
        assert!(err.contains("require-own"), "it names the set: {err}");

        let c: RoutingConfig = toml::from_str(
            "[[route]]\nmatch = { label = \"x\" }\nworkspace = \"w\"\nrepo = \"/tmp\"\n\
             runtime = \"claude\"\nbilling_guard = \"maybe\"\n",
        )
        .unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("billing_guard"), "{err}");
    }

    // ---- what a token costs, and what a plan does (gh#182) ---------------

    #[test]
    fn a_rate_override_writes_two_numbers_and_derives_the_cache_pair() {
        let c: RoutingConfig = toml::from_str(
            r#"
[defaults.rates."claude-opus-5"]
input = 4.0
output = 20.0

[defaults.rates."gpt-5.6-terra"]
input = 1.25
output = 10.0
cache_read = 0.125
cache_write = 1.25
"#,
        )
        .expect("parses");
        assert!(c.validate().is_ok());

        // Two numbers, and the cache halves at the published multipliers —
        // which is how the shipped table computes its own rows, and cannot be
        // got wrong by fat-fingering a 0.1.
        let opus = c.defaults.rates["claude-opus-5"].rate();
        assert_eq!(opus.input, Usd::from_dollars(4.0));
        assert_eq!(opus.cache_read, Usd::from_dollars(0.4));
        assert_eq!(opus.cache_write, Usd::from_dollars(5.0));
        // Four when a provider prices them differently.
        let gpt = c.defaults.rates["gpt-5.6-terra"].rate();
        assert_eq!(gpt.cache_read, Usd::from_dollars(0.125));
        assert_eq!(gpt.cache_write, Usd::from_dollars(1.25));

        // And the board prices with them: the override wins over the shipped
        // row, and the model the table never heard of is now priceable.
        let prices = crate::prices::Prices::from_config(&c);
        assert_eq!(
            prices.table.rate_for("claude-opus-5").unwrap().rate.input,
            Usd::from_dollars(4.0)
        );
        assert!(prices.table.rate_for("gpt-5.6-terra").is_some());
    }

    /// Money is believed. A priced page has no plausibility check of its own,
    /// so a negative rate has to be refused where every other typo is.
    #[test]
    fn a_negative_rate_or_plan_is_refused_by_name() {
        let c: RoutingConfig =
            toml::from_str("[defaults.rates.\"claude-opus-5\"]\ninput = -5.0\noutput = 25.0\n")
                .expect("parses");
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("[defaults.rates.\"claude-opus-5\"]"), "{err}");
        assert!(err.contains("cannot be negative"), "{err}");

        let c: RoutingConfig =
            toml::from_str("[account.\"brede@tally.no\"]\nmonthly_usd = -200\n").expect("parses");
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("[account.\"brede@tally.no\"]"), "{err}");
    }

    #[test]
    fn a_plan_is_written_beside_the_slot_it_describes() {
        let c: RoutingConfig = toml::from_str(
            r#"
[account."8f2c1d0a7b6e4539"]
email = "brede@tally.no"
plan = "Claude Max 20x"
monthly_usd = 200

[account."ana@example.com"]
monthly_usd = 19.99
"#,
        )
        .expect("parses");
        assert!(c.validate().is_ok());
        // Written as an integer or with cents — both are amounts a person
        // types, and refusing one would be a papercut with no upside.
        assert_eq!(
            c.accounts["8f2c1d0a7b6e4539"].monthly_usd,
            Usd::from_dollars(200.0)
        );
        assert_eq!(
            c.accounts["ana@example.com"].monthly_usd,
            Usd::from_dollars(19.99)
        );
        assert_eq!(
            c.accounts["8f2c1d0a7b6e4539"].email.as_deref(),
            Some("brede@tally.no")
        );
        // A board told nothing knows nothing: unconfigured is not zero.
        assert!(RoutingConfig::default().accounts.is_empty());
    }

    /// The board loop rebuilds itself by comparing the config it is running
    /// against the one on disk (gh#189), so a rate change has to be visible to
    /// that comparison — a table nobody noticed changing would price the page
    /// at yesterday's rates until the next restart.
    #[test]
    fn a_changed_rate_is_a_changed_config() {
        let before: RoutingConfig =
            toml::from_str("[defaults.rates.\"claude-opus-5\"]\ninput = 5.0\noutput = 25.0\n")
                .unwrap();
        let after: RoutingConfig =
            toml::from_str("[defaults.rates.\"claude-opus-5\"]\ninput = 4.0\noutput = 25.0\n")
                .unwrap();
        assert_ne!(before, after);
        let plan: RoutingConfig =
            toml::from_str("[account.\"brede@tally.no\"]\nmonthly_usd = 200\n").unwrap();
        assert_ne!(plan, RoutingConfig::default());
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

    // ---- the memory floor (gh#533) ---------------------------------------

    #[test]
    fn the_memory_floor_defaults_to_fifteen_percent_of_the_box() {
        let c = RoutingConfig::default();
        assert_eq!(c.defaults.min_memory_headroom, "15%");
        assert_eq!(
            c.min_memory_headroom(),
            Some(crate::pressure::DEFAULT_HEADROOM)
        );
    }

    #[test]
    fn the_memory_floor_is_written_as_a_percentage_and_turned_off_by_name() {
        assert_eq!(parse_memory_headroom("25%"), Ok(Some(0.25)));
        // The `%` is how anybody writes it, and leaving it off means the same.
        assert_eq!(parse_memory_headroom(" 25 "), Ok(Some(0.25)));
        for spelling in ["off", "OFF", "none", "never", "0%", "0"] {
            assert_eq!(parse_memory_headroom(spelling), Ok(None), "{spelling}");
        }
    }

    /// A floor no box can ever be over is a board that dispatches nothing, and
    /// finding that out at 02:00 is the failure this guard exists to prevent —
    /// so it is refused where every other typo is, rather than clamped.
    #[test]
    fn a_floor_no_box_could_clear_is_refused_rather_than_clamped() {
        assert!(parse_memory_headroom("100%").is_err());
        assert!(parse_memory_headroom("150%").is_err());
        // …and a negative one is a typo, never "off".
        assert!(parse_memory_headroom("-5%").is_err());
        let c: RoutingConfig =
            toml::from_str("[defaults]\nmin_memory_headroom = \"100%\"\n").unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("[defaults] min_memory_headroom"), "{err}");
    }

    #[test]
    fn a_mistyped_floor_is_refused_rather_than_read_as_off() {
        assert!(parse_memory_headroom("a lot").is_err());
        let c: RoutingConfig =
            toml::from_str("[defaults]\nmin_memory_headroom = \"plenty\"\n").unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("[defaults] min_memory_headroom"), "{err}");
        assert!(err.contains("plenty"), "it names the offender: {err}");
        // …and a config that reached the accessor with one falls back to the
        // default floor, never to no gate at all.
        assert_eq!(
            c.min_memory_headroom(),
            Some(crate::pressure::DEFAULT_HEADROOM)
        );
    }

    /// The gate off must never reach `/proc`: `headroom()` is the only caller
    /// that reads the box, and `off` is answered before it does.
    #[test]
    fn a_gate_that_is_off_holds_no_dispatch() {
        let c = github("[defaults]\nmin_memory_headroom = \"off\"\n");
        assert_eq!(c.min_memory_headroom(), None);
        assert_eq!(c.headroom(), crate::pressure::Headroom::Unknown);
        assert!(crate::dispatch::check_pressure(&c.headroom()).is_ok());
    }

    // ---- the turn guardrails (gh#270) ------------------------------------

    #[test]
    fn a_board_that_says_nothing_still_guards_a_spinning_turn() {
        // Both halves have to be real numbers by default, for the reason the
        // duration cap does: before this, nothing bounded a run that was
        // failing at full speed, and it spent the whole two hours doing it.
        let limits = RoutingConfig::default().turn_limits(None);
        assert_eq!(limits.tool_failures, Some(10));
        assert_eq!(limits.tool_calls, Some(2000));
    }

    #[test]
    fn a_route_sets_its_own_guardrails_over_the_defaults() {
        // What counts as a normal number of failures is a property of the
        // work — a route whose suite is flaky under load fails a lot without
        // being stuck.
        let c = github(
            r#"
[defaults]
max_tool_failures = "10"
max_tool_calls = "2000"

[[route]]
match = { label = "flaky" }
workspace = "w"
repo = "/tmp"
runtime = "claude"
max_tool_failures = "40"

[[route]]
match = { label = "quiet" }
workspace = "w"
repo = "/tmp"
runtime = "claude"
max_tool_calls = "off"
"#,
        );
        let flaky = c.turn_limits(Some(&c.routes[0]));
        assert_eq!(flaky.tool_failures, Some(40));
        // The half it did not override still comes from `[defaults]`.
        assert_eq!(flaky.tool_calls, Some(2000));
        let quiet = c.turn_limits(Some(&c.routes[1]));
        assert_eq!(quiet.tool_failures, Some(10));
        assert_eq!(quiet.tool_calls, None);
        // A route deleted from under a live attempt falls back to the
        // defaults, never to unbounded.
        assert_eq!(c.turn_limits(None).tool_failures, Some(10));
    }

    // ---- the instruction file (gh#272) ------------------------------------

    #[test]
    fn a_board_that_says_nothing_hands_its_agents_the_conventions() {
        // On by default, because the thing it replaces is a Codex dispatch that
        // had never heard of the board that started it.
        assert!(RoutingConfig::default().agent_instructions(None));
    }

    #[test]
    fn a_route_can_keep_its_account_out_of_it() {
        let c = github(
            r#"
[defaults]
agent_instructions = true

[[route]]
match = { label = "theirs" }
workspace = "w"
repo = "/tmp"
runtime = "codex"
account = "0011223344556677"
agent_instructions = false

[[route]]
match = { label = "ours" }
workspace = "w"
repo = "/tmp"
runtime = "claude"
"#,
        );
        assert!(!c.agent_instructions(Some(&c.routes[0])));
        assert!(c.agent_instructions(Some(&c.routes[1])));
        // And a board that turns it off everywhere turns it off for the route
        // that never mentioned it.
        let off = github("[defaults]\nagent_instructions = false\n");
        assert!(!off.agent_instructions(None));
    }

    // ---- MCP servers (gh#273) --------------------------------------------

    #[test]
    fn a_board_that_says_nothing_gives_dispatches_the_board_server() {
        assert_eq!(
            RoutingConfig::default().mcp_servers(None),
            [comet_proto::McpServer {
                name: "comet-board".into(),
                command: "comet-board".into(),
                args: vec!["mcp".into()],
            }]
        );
    }

    #[test]
    fn a_route_replaces_or_disables_the_default_mcp_list() {
        let c = github(
            r#"
[defaults]
mcp_servers = [{ name = "board", command = "comet-board", args = ["mcp"] }]

[[route]]
match = { label = "custom" }
workspace = "w"
repo = "/tmp"
runtime = "codex"
mcp_servers = [{ name = "repo", command = "repo-mcp", args = ["--stdio"] }]

[[route]]
match = { label = "none" }
workspace = "w"
repo = "/tmp"
runtime = "claude"
mcp_servers = []

[[route]]
workspace = "w"
repo = "/tmp"
runtime = "opencode"
"#,
        );
        assert_eq!(c.mcp_servers(Some(&c.routes[0]))[0].name, "repo");
        assert!(c.mcp_servers(Some(&c.routes[1])).is_empty());
        assert_eq!(c.mcp_servers(Some(&c.routes[2]))[0].name, "board");
    }

    #[test]
    fn invalid_or_colliding_mcp_names_are_refused_before_dispatch() {
        let c: RoutingConfig = toml::from_str(
            r#"
[defaults]
mcp_servers = [
  { name = "same-name", command = "one" },
  { name = "same_name", command = "two" },
  { name = " bad ", command = "" },
]
"#,
        )
        .unwrap();
        let problems = c.problems().join("\n");
        assert!(problems.contains("collides"), "{problems}");
        assert!(problems.contains("use only letters"), "{problems}");
        assert!(problems.contains("empty command"), "{problems}");
    }

    #[test]
    fn off_is_how_a_guardrail_is_removed() {
        for spelling in ["off", "OFF", "none", "never", "0"] {
            let c = github(&format!(
                "[defaults]\nmax_tool_failures = \"{spelling}\"\nmax_tool_calls = \"{spelling}\"\n"
            ));
            assert!(c.turn_limits(None).is_off(), "{spelling}");
        }
    }

    #[test]
    fn a_guardrail_too_tight_to_be_useful_is_raised_to_its_floor() {
        // A cap of one would steer the first run whose grep missed.
        assert_eq!(
            parse_turn_limit("1", crate::spin::MIN_TOOL_FAILURES),
            Ok(Some(3))
        );
        assert_eq!(
            parse_turn_limit("12", crate::spin::MIN_TOOL_FAILURES),
            Ok(Some(12))
        );
        assert_eq!(
            parse_turn_limit("4", crate::spin::MIN_TOOL_CALLS),
            Ok(Some(50))
        );
    }

    #[test]
    fn a_mistyped_guardrail_is_refused_rather_than_read_as_unbounded() {
        let c: RoutingConfig = toml::from_str(
            "[[route]]\nmatch = { label = \"x\" }\nworkspace = \"w\"\nrepo = \"/tmp\"\n\
             runtime = \"claude\"\nmax_tool_failures = \"a few\"\n",
        )
        .unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("max_tool_failures"), "{err}");
        assert!(err.contains("a few"), "it names the offender: {err}");

        let c: RoutingConfig = toml::from_str("[defaults]\nmax_tool_calls = \"lots\"\n").unwrap();
        assert!(
            c.validate()
                .unwrap_err()
                .to_string()
                .contains("[defaults] max_tool_calls")
        );
    }

    // ---- the chat shelf (gh#139) -----------------------------------------

    #[test]
    fn a_settled_chat_leaves_the_shelf_with_no_window_at_all() {
        // gh#139 gave the chat the checkout's week. The operator reading the
        // shelf the morning after (2026-08-08): thirteen finished rows under
        // one space, all of one night's merged work — "having the issues alive
        // and not collected is kinda worthless really". The guards in
        // `gc::chat_standing` are what protect an unfinished chat; a window on
        // top of them only delays the ones that are finished.
        let c = RoutingConfig::default();
        assert_eq!(c.archive_chats_secs(None), Some(0));
        // The checkout keeps its week — it is evidence, not a row in a list.
        assert_eq!(c.retain_worktrees_secs(), Some(7 * 86_400));
    }

    #[test]
    fn on_settle_is_a_spelling_and_a_bare_zero_is_not() {
        for spelling in ["on-settle", "on settle", "immediately", "NOW"] {
            assert_eq!(parse_chat_retention(spelling), Ok(Some(0)), "{spelling}");
        }
        // `0` means "off, forever" for a worktree and "no window" for a chat.
        // Opposite readings of one character: refuse rather than guess.
        for zero in ["0", "0s", "0d"] {
            let err = parse_chat_retention(zero).expect_err(zero);
            assert!(err.contains("on-settle") && err.contains("off"), "{err}");
        }
        assert_eq!(parse_retention("0"), Ok(None), "unchanged for worktrees");
        // A grace period is still a grace period.
        assert_eq!(parse_chat_retention("2d"), Ok(Some(2 * 86_400)));
        assert_eq!(parse_chat_retention("off"), Ok(None));
    }

    // ---- the build output inside the checkout (gh#186) --------------------

    /// The three windows a dispatch's leavings age on, and why they differ. The
    /// checkout keeps its week because somebody may go back for 14 MB; the cache
    /// inside it keeps nothing, because at 20–36 GB a week of them does not fit
    /// on the disk and nothing reads them after the run.
    #[test]
    fn build_output_and_the_checkout_holding_it_are_on_different_clocks() {
        let c = RoutingConfig::default();
        assert_eq!(c.retain_worktrees_secs(), Some(7 * 86_400));
        assert_eq!(c.retain_build_output_secs(), Some(0));
        assert_eq!(c.archive_chats_secs(None), Some(0));
    }

    #[test]
    fn the_build_output_window_is_a_setting_and_never_silently_off() {
        for spelling in ["on-settle", "immediately", "NOW"] {
            assert_eq!(parse_build_retention(spelling), Ok(Some(0)), "{spelling}");
        }
        assert_eq!(parse_build_retention("2h"), Ok(Some(2 * 3_600)));
        assert_eq!(parse_build_retention("off"), Ok(None));
        // Same opposite readings of one character as the shelf's, refused for
        // the same reason.
        for zero in ["0", "0h"] {
            let err = parse_build_retention(zero).expect_err(zero);
            assert!(err.contains("on-settle") && err.contains("off"), "{err}");
        }
        // A typo is refused by validation rather than read as "keep forever" —
        // failing towards a full disk is the bug this key exists to fix.
        let c: RoutingConfig =
            toml::from_str("[defaults]\nretain_build_output = \"a while\"\n").unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("[defaults] retain_build_output"), "{err}");
        // …and a config that reached the sweep with one anyway sweeps on the
        // default window, never on "never".
        assert_eq!(c.retain_build_output_secs(), Some(0));
    }

    #[test]
    fn a_route_sets_its_own_shelf_window_over_the_default() {
        let c = github(
            r#"
[defaults]
archive_chats = "3d"

[[route]]
match = { label = "keep" }
workspace = "w"
repo = "/tmp"
runtime = "claude"
archive_chats = "30d"

[[route]]
match = { label = "scratch" }
workspace = "w"
repo = "/tmp"
runtime = "claude"
archive_chats = "off"

[[route]]
match = { label = "plain" }
workspace = "w"
repo = "/tmp"
runtime = "claude"
"#,
        );
        assert_eq!(c.archive_chats_secs(Some(&c.routes[0])), Some(30 * 86_400));
        assert_eq!(
            c.archive_chats_secs(Some(&c.routes[1])),
            None,
            "a route may opt out of archiving entirely"
        );
        assert_eq!(c.archive_chats_secs(Some(&c.routes[2])), Some(3 * 86_400));
        // A route renamed or deleted under a finished attempt falls back to the
        // board-wide window, never to "never".
        assert_eq!(c.archive_chats_secs(None), Some(3 * 86_400));
    }

    #[test]
    fn off_is_how_chat_archiving_is_turned_off() {
        // `0` is no longer among these: it is the one spelling that reads as
        // both ends of this setting, so it is refused — see
        // `on_settle_is_a_spelling_and_a_bare_zero_is_not`.
        for spelling in ["off", "OFF", "none", "never"] {
            let c = github(&format!("[defaults]\narchive_chats = \"{spelling}\"\n"));
            assert_eq!(c.archive_chats_secs(None), None, "{spelling}");
        }
    }

    #[test]
    fn a_mistyped_shelf_window_is_refused_rather_than_read_as_forever() {
        // The typo and the deliberate `off` behave identically, and only one of
        // them is what somebody meant — so it is a config error.
        let c: RoutingConfig = toml::from_str(
            "[[route]]\nmatch = { label = \"x\" }\nworkspace = \"w\"\nrepo = \"/tmp\"\n\
             runtime = \"claude\"\narchive_chats = \"a fortnight\"\n",
        )
        .unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("archive_chats"), "{err}");
        assert!(err.contains("a fortnight"), "it names the offender: {err}");

        let c: RoutingConfig = toml::from_str("[defaults]\narchive_chats = \"someday\"\n").unwrap();
        assert!(
            c.validate()
                .unwrap_err()
                .to_string()
                .contains("[defaults] archive_chats")
        );
    }

    #[test]
    fn interval_has_a_floor() {
        let s = SyncConfig {
            interval: "0s".into(),
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
        std::fs::write(paths.env_file(), "GITHUB_TOKEN=first\n").unwrap();
        let first = Credentials::load_with(&paths, no_inherited);
        assert_eq!(first.github_token.as_deref(), Some("first"));
        assert_eq!(first.github_app_id, None);

        std::fs::write(
            paths.env_file(),
            "GITHUB_TOKEN=second\nGITHUB_APP_ID=42\n",
        )
        .unwrap();
        let edited = Credentials::load_with(&paths, no_inherited);
        assert_eq!(edited.github_token.as_deref(), Some("second"));
        assert_eq!(edited.github_app_id.as_deref(), Some("42"));

        std::fs::write(paths.env_file(), "GITHUB_TOKEN=second\n").unwrap();
        let removed = Credentials::load_with(&paths, no_inherited);
        assert_eq!(removed.github_app_id, None);

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
            "GITHUB_APP_ID=file-app\nGITHUB_TOKEN=file-github\n",
        )
        .unwrap();

        let credentials = Credentials::load_with(&paths, |key| match key {
            "GITHUB_APP_ID" => Ok("shell-app".to_string()),
            "GITHUB_TOKEN" => Ok(String::new()),
            _ => Err(std::env::VarError::NotPresent),
        });
        assert_eq!(credentials.github_app_id.as_deref(), Some("shell-app"));
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
    /// identical (gh#96). The box wizard writes `GITHUB_TOKEN=` when the
    /// stage is skipped with Enter, and an empty string that reads as
    /// "configured" turns a deliberate skip into a credential the board tries
    /// to authenticate with — and, for the App pair, into a "half configured"
    /// failure over two keys nobody set.
    #[test]
    fn an_empty_value_reads_as_never_configured() {
        let c = creds("GITHUB_TOKEN=\nGITHUB_APP_ID=\nGITHUB_APP_PRIVATE_KEY_PATH=\n");
        assert_eq!(c.github_auth(), GithubAuth::None);
        assert_eq!(c.github_app_half_configured(), None);

        // `KEY=""` is the same skip, written by a wizard that quotes.
        assert_eq!(creds("GITHUB_TOKEN=\"\"\n").github_token, None);

        // And one real key beside a skipped one is still one real key.
        let c = creds("GITHUB_APP_ID=\nGITHUB_TOKEN=ghp_real\n");
        assert_eq!(c.github_app_id, None);
        assert_eq!(c.github_auth(), GithubAuth::Token("ghp_real".into()));
    }

    /// Which GitHub account the map says somebody is (gh#369). Only an address
    /// GitHub minted answers: it is the one form that names an account by
    /// construction, and choosing whose credential casts a review is not a
    /// question to answer by inference.
    #[test]
    fn only_a_github_minted_address_says_which_account_a_member_is() {
        let mut cfg = RoutingConfig::default();
        cfg.users.insert(
            "ana@example.com".into(),
            "22494697+ana@users.noreply.github.com".into(),
        );
        cfg.users.insert(
            "sam@example.com".into(),
            "Sam Ito <sam@work.example>".into(),
        );
        assert_eq!(
            cfg.github_login_for("ana@example.com").as_deref(),
            Some("ana")
        );
        // Case-insensitive on the key, exactly as the author lookup is: the
        // address arrives from whichever frontend they signed in on.
        assert_eq!(
            cfg.github_login_for("ANA@example.com").as_deref(),
            Some("ana")
        );
        // A perfectly good commit author, and no account name in it — Sam
        // reviews as the board until his entry says which login he is.
        assert_eq!(cfg.github_login_for("sam@example.com"), None);
        assert_eq!(cfg.github_login_for("nobody@example.com"), None);
        assert_eq!(cfg.github_login_for(""), None);
    }

    /// A member's own review credential (gh#369): named after the account it
    /// belongs to, so a box holding several does not have to be told which is
    /// whose, and read from the same file with the same precedence as every
    /// other secret.
    #[test]
    fn a_members_review_token_is_read_off_the_login_it_belongs_to() {
        assert_eq!(user_token_env("ana"), "GITHUB_USER_TOKEN_ANA");
        // The one character a login may hold and an environment variable may
        // not. `a_b` is not a login anybody can have, so nothing else can
        // normalise onto this key.
        assert_eq!(user_token_env("octo-cat"), "GITHUB_USER_TOKEN_OCTO_CAT");

        let c = creds(
            "GITHUB_TOKEN=ghp_board\nGITHUB_USER_TOKEN_ANA=ghu_ana\n\
             GITHUB_USER_TOKEN_OCTO_CAT=ghu_octo\n",
        );
        assert_eq!(c.user_token("ana"), Some("ghu_ana"));
        // GitHub does not care how the login is cased, and neither does the
        // lookup: the address in `[users]` is whatever somebody pasted.
        assert_eq!(c.user_token("Ana"), Some("ghu_ana"));
        assert_eq!(c.user_token("octo-cat"), Some("ghu_octo"));
        assert_eq!(
            c.user_token("sam"),
            None,
            "a member with no token of their own"
        );
        assert_eq!(
            c.user_token_logins().collect::<Vec<_>>(),
            vec!["ANA", "OCTO_CAT"],
        );
        // The board's own credential is untouched by any of this — one of them
        // opens pull requests and the other reviews them, and they must not be
        // able to become the same string by accident.
        assert_eq!(c.github_auth(), GithubAuth::Token("ghp_board".into()));

        // A skipped value is not a credential, here as everywhere (gh#96), and
        // a key with nothing after the prefix names nobody.
        let empty = creds("GITHUB_USER_TOKEN_ANA=\nGITHUB_USER_TOKEN_=ghu_nobody\n");
        assert_eq!(empty.user_token("ana"), None);
        assert_eq!(empty.user_token_logins().count(), 0);
    }

    /// The shell wins over the file, the way it does for one key — including
    /// the emptied variable, which takes the file's value away rather than
    /// shadowing it with nothing.
    #[test]
    fn a_review_token_in_the_shell_overrides_the_file() {
        let dir = std::env::temp_dir().join(format!(
            "hb-user-token-{}-{}",
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
            "GITHUB_USER_TOKEN_ANA=from-file\nGITHUB_USER_TOKEN_SAM=from-file\n",
        )
        .unwrap();
        let c = Credentials::load_with_env(
            &paths,
            |_| Err(std::env::VarError::NotPresent),
            [
                (
                    "GITHUB_USER_TOKEN_ANA".to_string(),
                    "from-shell".to_string(),
                ),
                ("GITHUB_USER_TOKEN_SAM".to_string(), String::new()),
                ("PATH".to_string(), "/usr/bin".to_string()),
            ],
        );
        assert_eq!(c.user_token("ana"), Some("from-shell"));
        assert_eq!(c.user_token("sam"), None);
        let _ = std::fs::remove_dir_all(dir);
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

    // ---- auto-pick rules (gh#490) --------------------------------------

    /// A full `[[automation]]` block parses into the rule the planner reads,
    /// with the defaults where nothing was written.
    #[test]
    fn an_automation_block_parses_with_its_defaults() {
        let cfg: RoutingConfig = toml::from_str(
            r#"
            [[automation]]
            name = "approved-maintenance"
            enabled = true
            owner = "Brede"
            labels = ["auto"]
            exclude_labels = ["blocked-on-human"]
            account = "slot-1"
            "#,
        )
        .unwrap();
        let a = &cfg.automations[0];
        assert_eq!(a.name, "approved-maintenance");
        assert!(a.enabled);
        assert_eq!(a.owner.as_deref(), Some("Brede"));
        assert_eq!(a.labels, vec!["auto"]);
        assert_eq!(a.max_per_eval, 1);
        assert_eq!(a.max_concurrent, 1);
        assert_eq!(a.daily_budget, None);
        assert_eq!(a.cooldown, "30m");
        assert_eq!(a.cooldown_secs(), Some(30 * 60));
        assert!(cfg.problems().is_empty(), "{:?}", cfg.problems());
    }

    /// Enabling is gated on the facts an unattended dispatch cannot do
    /// without: a named owner and at least one required label. A *disabled*
    /// rule may be half-written — that is what the settings page creates.
    #[test]
    fn an_enabled_rule_without_owner_or_labels_is_refused() {
        let cfg: RoutingConfig = toml::from_str(
            r#"
            [[automation]]
            name = "hasty"
            enabled = true
            "#,
        )
        .unwrap();
        let problems = cfg.problems();
        assert!(problems.iter().any(|p| p.contains("no owner")), "{problems:?}");
        assert!(
            problems.iter().any(|p| p.contains("no required labels")),
            "{problems:?}"
        );

        let drafted: RoutingConfig = toml::from_str(
            r#"
            [[automation]]
            name = "drafted"
            "#,
        )
        .unwrap();
        assert!(drafted.problems().is_empty(), "{:?}", drafted.problems());
    }

    /// The always-on checks: names must be unique (case-insensitively — one
    /// history per rule), a cooldown typo must not read as the default, and a
    /// runtime or source that means nothing is said now rather than at 02:00.
    #[test]
    fn automation_names_cooldowns_runtimes_and_sources_are_checked() {
        let cfg: RoutingConfig = toml::from_str(
            r#"
            [[automation]]
            name = "dup"
            cooldown = "shortly"
            runtime = "codx"
            source = "jira"

            [[automation]]
            name = "DUP"
            "#,
        )
        .unwrap();
        let problems = cfg.problems();
        assert!(problems.iter().any(|p| p.contains("appears twice")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("cooldown")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("`codx`")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("`jira`")), "{problems:?}");
    }
}
