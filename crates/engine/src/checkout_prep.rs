//! Checkout preparation (gh#422) — the step between "git gave us a checkout"
//! and "an agent may start in it".
//!
//! `git worktree add` finishes in milliseconds and leaves an environment that
//! cannot build anything: no `node_modules`, no generated code, no
//! machine-local `.env.local`, and no written-down answer to "what is the
//! command that runs this thing". Every dispatched agent rediscovered that in
//! prose, and every rediscovery was billed. This module is the repository's own
//! answer to it, in a file the repository reviews:
//!
//! ```toml
//! # .comet/repo.toml
//! version = 1
//!
//! [setup]
//! run = "scripts/setup.sh"
//! timeout = "10m"
//!
//! [run]
//! run = "cargo run -p comet"
//!
//! [archive]
//! run = "cargo clean"
//!
//! [env]
//! RUST_BACKTRACE = "1"
//!
//! [[link]]
//! from = "dev.env"
//! to = ".env.local"
//! ```
//!
//! Four rules hold the whole thing up.
//!
//! **The repository names a leaf; comet names the root.** A `[[link]]`'s `from`
//! is resolved under [`CheckoutPrep::locals_root`] —
//! `{data_dir}/locals/{repository identity}/`, a directory the *operator* fills by hand —
//! and it may not be absolute and may not contain `..`. This is the deliberate
//! divergence from the implementation this was researched against, which
//! discovers `.env` files by walking the machine. comet runs shared boxes and
//! dispatches work for teammates: a file in a repository that anybody can open
//! a pull request against must never be able to address `~/.ssh/id_ed25519`,
//! and the way to guarantee that is to make the path *unspellable* rather than
//! to filter it. What each projection reached is written into the record
//! ([`LinkOutcome`]) before the agent starts, so the credential reach of a
//! checkout is a thing you can read rather than infer.
//!
//! **Preparation is a property of the checkout, not of the attempt.** The
//! record lives under `{data_dir}/checkout-prep/{hash of the worktree path}/`,
//! survives the attempt that caused it, and short-circuits on a second visit
//! when the recipe has not changed. That is what makes a retry cheap and what
//! makes an ordinary (non-board) session and a board dispatch the same code
//! path — the board composes this, it does not own it.
//!
//! **A failure is a record, not an exception.** `prepare` returns a
//! [`PrepRecord`] whichever way it went; a malformed recipe, a link that could
//! not be projected and a setup script that exited 1 all land as
//! [`PrepState::Failed`] with the sentence and the log path on them. The
//! caller's job is then simple and always the same: do not start the agent.
//!
//! **Everything is bounded.** The setup step runs in its own process group with
//! a timeout the recipe may shorten but not lift past [`MAX_TIMEOUT`], its
//! output is capped at [`LOG_CAP_BYTES`] with the head kept and the truncation
//! said out loud, and a [`CancellationToken`] kills the group rather than the
//! shell — a `npm install` whose parent `sh` was killed is otherwise still
//! running an hour later.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use comet_harness::CancellationToken;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

/// The repository-owned recipe, relative to the worktree root.
///
/// A directory rather than a bare `comet.toml` at the root: gh#273's
/// per-repository MCP configuration wants a home beside this one, and a repo
/// that has to accept one comet file will not thank us for accepting three.
pub const RECIPE_PATH: &str = ".comet/repo.toml";

/// The only schema version this engine executes. A file naming another one is
/// refused rather than best-guessed — the failure mode this issue exists to
/// remove is a setup that silently did less than the repository asked.
pub const RECIPE_VERSION: u32 = 1;

/// How long a setup step may run when the recipe does not say.
pub const DEFAULT_SETUP_TIMEOUT: Duration = Duration::from_secs(600);
/// How long an archive step may run when the recipe does not say. Shorter than
/// setup on purpose: archiving happens during reclamation, on a box whose disk
/// is already the problem, and a `clean` that hangs must not hold the sweep.
pub const DEFAULT_ARCHIVE_TIMEOUT: Duration = Duration::from_secs(300);
/// The ceiling a recipe may not raise. An unbounded setup is an attempt that
/// never starts and never fails, which is the one outcome nobody can act on.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(3600);

/// How much setup output is kept. The head, not the tail: the first error is
/// what diagnoses a broken setup, and a 200MB `npm install` log that pushed it
/// out is a file nobody can read anyway.
pub const LOG_CAP_BYTES: usize = 256 * 1024;

/// Environment names a recipe may not set, and why each one is here.
///
/// `COMET_*` is the sharp one. A dispatched agent's environment carries board
/// routing variables so its CLI attaches to the board that dispatched it. A
/// repository file that could replace those could point a teammate's agent at
/// another board — a file in a pull request deciding which database the box
/// writes to. The loader variables are the classic pair: `LD_PRELOAD` and
/// `DYLD_INSERT_LIBRARIES` turn "run the setup script" into "inject into
/// everything the setup script starts". `PATH` is refused because a recipe that
/// wants a different toolchain should say so in its script, where a reviewer
/// reads it, rather than by silently re-pointing every binary name in it.
const RESERVED_ENV_PREFIXES: &[&str] = &["COMET_", "DYLD_"];
const RESERVED_ENV_NAMES: &[&str] = &["PATH", "LD_PRELOAD", "LD_LIBRARY_PATH", "GIT_ASKPASS"];

// ── the recipe ──────────────────────────────────────────────────────────────

/// One command the repository owns, with its own bound.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// The command line, run by `sh -c` from the worktree root.
    pub run: String,
    /// `"10m"`, `"90s"`, `"1h"`. Absent = the caller's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<String>,
}

/// A machine-local file the repository asks for, by leaf name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Link {
    /// Relative to [`CheckoutPrep::locals_root`]. Never absolute, never `..`.
    pub from: String,
    /// Relative to the worktree root. Never absolute, never `..`.
    pub to: String,
}

/// `.comet/repo.toml`, parsed and validated.
///
/// Unknown keys are refused rather than ignored. A silently-dropped `timout =
/// "20m"` is precisely the class of failure this issue is about; forward
/// compatibility is `version`'s job, and a file from a later schema says so in
/// one line instead of half-running.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub version: u32,
    /// Idempotent preparation, run after the worktree exists and before an
    /// agent does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<Step>,
    /// The canonical development command. Never started by preparation —
    /// "started explicitly rather than guessed" is the point of writing it
    /// down. Served to callers so a viewport can offer it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<Step>,
    /// Bounded cleanup of reproducible output, run before reclamation
    /// (gh#186's sweep) so a repo that knows how to clean itself gets to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive: Option<Step>,
    /// Values safe to commit — they are in the repository, in the open.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, rename = "link", skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
    /// gh#273's seam. Parsed so that a repository which fills it in early is
    /// not a *parse error* on an engine that predates the feature, and
    /// deliberately not acted on here: what an MCP server is allowed to be on a
    /// shared box is that issue's question, not this one's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<toml::Value>,
}

impl Recipe {
    /// Parse and validate. The error is what a repository author reads, so it
    /// names the key.
    pub fn parse(text: &str) -> Result<Recipe, String> {
        let recipe: Recipe = toml::from_str(text).map_err(|e| {
            // toml's own message carries the span; the prefix says which file
            // it is, because the reader is holding a dispatch that refused.
            format!("{RECIPE_PATH} is not valid: {e}")
        })?;
        recipe.validate()?;
        Ok(recipe)
    }

    fn validate(&self) -> Result<(), String> {
        if self.version != RECIPE_VERSION {
            return Err(format!(
                "{RECIPE_PATH} says version = {}, and this comet understands \
                 version {RECIPE_VERSION}",
                self.version
            ));
        }
        for (name, step) in [
            ("setup", &self.setup),
            ("run", &self.run),
            ("archive", &self.archive),
        ] {
            let Some(step) = step else { continue };
            if step.run.trim().is_empty() {
                return Err(format!("[{name}] has an empty `run`"));
            }
            if let Some(spec) = &step.timeout {
                let d = parse_duration(spec)
                    .ok_or_else(|| format!("[{name}] timeout `{spec}` is not a duration"))?;
                if d > MAX_TIMEOUT {
                    return Err(format!(
                        "[{name}] timeout `{spec}` is longer than the {} the engine allows",
                        format_duration(MAX_TIMEOUT)
                    ));
                }
                if d.is_zero() {
                    return Err(format!("[{name}] timeout `{spec}` is zero"));
                }
            }
        }
        for name in self.env.keys() {
            if let Some(why) = reserved_env(name) {
                return Err(format!("[env] may not set `{name}` — {why}"));
            }
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
            {
                return Err(format!("[env] name `{name}` is not an environment name"));
            }
        }
        for link in &self.links {
            relative_within("from", &link.from)?;
            relative_within("to", &link.to)?;
        }
        Ok(())
    }

    /// The setup timeout this recipe asks for, already bounded.
    pub fn setup_timeout(&self) -> Duration {
        step_timeout(self.setup.as_ref(), DEFAULT_SETUP_TIMEOUT)
    }

    /// The archive timeout this recipe asks for, already bounded.
    pub fn archive_timeout(&self) -> Duration {
        step_timeout(self.archive.as_ref(), DEFAULT_ARCHIVE_TIMEOUT)
    }

    /// Whether accepting this recipe grants repository-authored effects to
    /// the host engine. `[run]` is excluded: it is only an offer, later run
    /// explicitly under the session's own sandbox. `[mcp]` is parsed-only.
    pub fn requires_host_approval(&self) -> bool {
        self.setup.is_some() || self.archive.is_some() || !self.links.is_empty()
    }
}

fn step_timeout(step: Option<&Step>, default: Duration) -> Duration {
    step.and_then(|s| s.timeout.as_deref())
        .and_then(parse_duration)
        .unwrap_or(default)
        .min(MAX_TIMEOUT)
}

/// Why a name is refused, phrased for the person holding the refusal.
fn reserved_env(name: &str) -> Option<&'static str> {
    if RESERVED_ENV_NAMES.contains(&name) {
        return Some("it decides how the box finds and authenticates its own tools");
    }
    if RESERVED_ENV_PREFIXES
        .iter()
        .any(|p| name.starts_with(p) && name.len() > p.len())
    {
        return Some(
            "names under this prefix address comet's own state and the dynamic \
             loader, and a file in a pull request must not be able to move either",
        );
    }
    None
}

/// The shared shape of both link halves: a relative path with no `..`, no root,
/// and no empty components. Enforced on the *spelling*, before anything is
/// resolved, so a refusal never depends on what happens to exist on the box.
fn relative_within(key: &str, raw: &str) -> Result<PathBuf, String> {
    let path = Path::new(raw);
    if raw.trim().is_empty() {
        return Err(format!("[[link]] `{key}` is empty"));
    }
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(format!(
                    "[[link]] `{key} = \"{raw}\"` leaves its root — `..` is not a \
                     path a repository file may spell"
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "[[link]] `{key} = \"{raw}\"` is absolute — a link names a leaf, \
                     and comet names the root it hangs from"
                ));
            }
        }
    }
    if out.as_os_str().is_empty() {
        return Err(format!("[[link]] `{key} = \"{raw}\"` names nothing"));
    }
    Ok(out)
}

/// `"10m"`, `"90s"`, `"1h"`, or a bare number of seconds.
fn parse_duration(spec: &str) -> Option<Duration> {
    let spec = spec.trim();
    let (digits, mult) = match spec.chars().last()? {
        's' => (&spec[..spec.len() - 1], 1),
        'm' => (&spec[..spec.len() - 1], 60),
        'h' => (&spec[..spec.len() - 1], 3600),
        _ => (spec, 1),
    };
    let n: u64 = digits.trim().parse().ok()?;
    Some(Duration::from_secs(n.checked_mul(mult)?))
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs.is_multiple_of(3600) {
        format!("{}h", secs / 3600)
    } else if secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

// ── the record ──────────────────────────────────────────────────────────────

/// Where a checkout is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrepState {
    /// The step is running. A record left in this state by a killed engine is
    /// treated as [`PrepState::Failed`] by [`CheckoutPrep::prepare`], which
    /// re-runs — setup is required to be idempotent precisely so that it can.
    Preparing,
    /// The checkout is ready for an agent. Also what a repository with no
    /// recipe gets, with [`PrepRecord::command`] empty.
    Ready,
    /// The checkout is not ready and the agent must not start.
    Failed,
}

impl PrepState {
    pub fn as_str(self) -> &'static str {
        match self {
            PrepState::Preparing => "preparing",
            PrepState::Ready => "ready",
            PrepState::Failed => "failed",
        }
    }
}

/// What one `[[link]]` actually did, kept so the credential reach of a checkout
/// is legible before the agent starts rather than after it has read something.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkOutcome {
    /// The box-local file, fully resolved.
    pub from: String,
    /// The path inside the checkout, repo-relative.
    pub to: String,
    /// `copied`, `kept` (the destination already existed), or `missing`.
    pub result: String,
    /// Unix mode of what was written, as `0644` — the fact somebody checks
    /// when the file is a credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

/// The lifecycle of one checkout, as it is persisted and as it is served.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrepRecord {
    pub state: PrepState,
    /// The checkout this is about, canonicalized.
    pub worktree: String,
    /// sha256 of the recipe file's bytes. The short-circuit key: a checkout
    /// that is `ready` for *this* digest needs nothing, and an edited recipe
    /// re-prepares without anybody having to remember to force it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe_digest: Option<String>,
    /// The setup command as run. `None` = there was no recipe, or it had no
    /// `[setup]`; the checkout is ready because nothing was asked of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub started_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// One sentence a human reads first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Absolute path to the captured output, when a step ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<LinkOutcome>,
    /// The canonical development command the recipe names, carried on the
    /// record so a viewport that has the status also has the offer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_command: Option<String>,
    /// A repository command exists, but this exact repository + recipe digest
    /// has not been approved in the engine-owned trust store. Links may still
    /// have been projected; no repository command ran.
    #[serde(default, skip_serializing_if = "is_false")]
    pub requires_approval: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl PrepRecord {
    /// Ready, because nothing was asked. The one path that writes no state:
    /// a box full of repositories with no recipe should grow no files.
    fn nothing_to_do(worktree: &Path, detail: &str) -> PrepRecord {
        PrepRecord {
            state: PrepState::Ready,
            worktree: worktree.to_string_lossy().into_owned(),
            recipe_digest: None,
            command: None,
            started_at: now_ms(),
            ended_at: Some(now_ms()),
            exit_code: None,
            detail: Some(detail.to_string()),
            log: None,
            links: Vec::new(),
            run_command: None,
            requires_approval: false,
        }
    }

    pub fn is_ready(&self) -> bool {
        self.state == PrepState::Ready
    }

    pub fn board_view(&self) -> comet_proto::view::board::CheckoutPreparation {
        use comet_proto::view::board::{
            CheckoutPreparation, CheckoutPreparationState, CheckoutProjection,
        };
        CheckoutPreparation {
            state: match self.state {
                PrepState::Preparing => CheckoutPreparationState::Preparing,
                PrepState::Ready => CheckoutPreparationState::Ready,
                PrepState::Failed => CheckoutPreparationState::Failed,
            },
            recipe_digest: self.recipe_digest.clone(),
            detail: self.detail.clone(),
            log: self.log.clone(),
            requires_approval: self.requires_approval,
            projections: self
                .links
                .iter()
                .map(|link| CheckoutProjection {
                    from: link.from.clone(),
                    to: link.to.clone(),
                    result: link.result.clone(),
                    mode: link.mode.clone(),
                })
                .collect(),
        }
    }

    /// The line the engine log and a transcript both want.
    pub fn summary(&self) -> String {
        match self.state {
            PrepState::Preparing => match &self.command {
                Some(cmd) => format!("preparing {} — `{cmd}`", self.worktree),
                None => format!("preparing {}", self.worktree),
            },
            PrepState::Ready => match &self.command {
                Some(cmd) => format!("{} is ready — `{cmd}` succeeded", self.worktree),
                None => format!(
                    "{} is ready — {}",
                    self.worktree,
                    self.detail.as_deref().unwrap_or("nothing to prepare")
                ),
            },
            PrepState::Failed => format!(
                "{} is not ready — {}",
                self.worktree,
                self.detail.as_deref().unwrap_or("preparation failed")
            ),
        }
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

// ── the primitive ───────────────────────────────────────────────────────────

/// What a caller wants prepared.
pub struct PrepareRequest<'a> {
    /// The checkout. Must exist.
    pub worktree: &'a Path,
    /// Stable, device-local identity of the repository the checkout was cut
    /// from. It keys both machine-local projections and command approval, so
    /// two unrelated `widget` repositories never share either namespace.
    pub repository_id: &'a str,
    /// Re-run even if the record says ready for this recipe. What a retry after
    /// a half-finished setup uses.
    pub force: bool,
    /// Kills the step's whole process group. `None` = no cancellation, still
    /// bounded by the timeout.
    pub cancel: Option<CancellationToken>,
}

/// Reads recipes, prepares checkouts, and remembers what happened.
#[derive(Clone)]
pub struct CheckoutPrep {
    data_dir: PathBuf,
    active: Arc<Mutex<HashMap<PathBuf, (String, CancellationToken)>>>,
    claims: Arc<Mutex<HashSet<PathBuf>>>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Approval {
    repository_id: String,
    recipe_digest: String,
    approved_at: i64,
}

/// Exclusive ownership of work waiting behind preparation. Dropping an
/// incomplete claim restores it; completing one removes it. The rename is the
/// durable state transition, so an engine restart can recover the same work.
pub struct ParkedClaim {
    prep: CheckoutPrep,
    key: PathBuf,
    parked: PathBuf,
    claimed: PathBuf,
    payload: String,
    completed: bool,
}

impl ParkedClaim {
    pub fn payload(&self) -> &str {
        &self.payload
    }

    pub fn complete(mut self) {
        let _ = std::fs::remove_file(&self.claimed);
        if let Some(parent) = self.claimed.parent() {
            let _ = sync_directory(parent);
        }
        self.completed = true;
    }
}

impl Drop for ParkedClaim {
    fn drop(&mut self) {
        if !self.completed && self.claimed.exists() && !self.parked.exists() {
            let _ = std::fs::rename(&self.claimed, &self.parked);
            if let Some(parent) = self.parked.parent() {
                let _ = sync_directory(parent);
            }
        }
        self.prep
            .claims
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.key);
    }
}

struct ActivePreparation {
    active: Arc<Mutex<HashMap<PathBuf, (String, CancellationToken)>>>,
    key: PathBuf,
    generation: String,
}

impl Drop for ActivePreparation {
    fn drop(&mut self) {
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        if active
            .get(&self.key)
            .is_some_and(|(generation, _)| generation == &self.generation)
        {
            active.remove(&self.key);
        }
    }
}

impl CheckoutPrep {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            active: Arc::new(Mutex::new(HashMap::new())),
            claims: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Where a `[[link]]`'s `from` is resolved: one directory per repository,
    /// under comet's own data dir, filled by the operator.
    ///
    /// The stable repository identity is intentionally opaque. A basename is
    /// not an identity: `owner-a/widget` and `owner-b/widget` must never share
    /// a secret namespace on a multi-tenant host.
    pub fn locals_root(&self, repository_id: &str) -> PathBuf {
        self.data_dir
            .join("locals")
            .join(identity_key(repository_id))
    }

    /// Per-checkout state: keyed by a hash of the canonical path so that a
    /// branch with slashes, a repo with spaces and a checkout under a symlinked
    /// home all get one flat, predictable directory.
    pub fn state_dir(&self, worktree: &Path) -> PathBuf {
        let canonical = canonical(worktree);
        let mut hasher = Sha256::new();
        hasher.update(canonical.to_string_lossy().as_bytes());
        let key = hex(&hasher.finalize());
        self.data_dir.join("checkout-prep").join(&key[..16])
    }

    fn record_path(&self, worktree: &Path) -> PathBuf {
        self.state_dir(worktree).join("prep.json")
    }

    fn log_path(&self, worktree: &Path) -> PathBuf {
        self.state_dir(worktree).join("prep.log")
    }

    fn brief_path(&self, worktree: &Path) -> PathBuf {
        self.state_dir(worktree).join("brief.json")
    }

    fn claimed_brief_path(&self, worktree: &Path) -> PathBuf {
        self.state_dir(worktree).join("brief.claimed.json")
    }

    fn approval_path(&self, repository_id: &str, recipe_digest: &str) -> PathBuf {
        self.data_dir
            .join("checkout-prep")
            .join("trust")
            .join(identity_key(repository_id))
            .join(format!("{recipe_digest}.json"))
    }

    /// The last thing known about this checkout, or `None` for one nothing has
    /// ever prepared.
    pub fn status(&self, worktree: &Path) -> Option<PrepRecord> {
        let text = std::fs::read_to_string(self.record_path(worktree)).ok()?;
        let mut record: PrepRecord = serde_json::from_str(&text).ok()?;
        if record.state == PrepState::Preparing {
            let key = canonical(worktree);
            let is_active = self
                .active
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&key);
            if !is_active {
                // The operation that wrote `preparing` no longer exists: the
                // engine restarted, its RPC future was dropped, or its task
                // was aborted. The process-group guard has already stopped
                // repository code; persist the actionable half of that fact.
                record.state = PrepState::Failed;
                record.ended_at = Some(now_ms());
                record.detail = Some(
                    "preparation was interrupted before it recorded completion; retry this checkout"
                        .to_string(),
                );
                return Some(self.write(record));
            }
        }
        Some(record)
    }

    /// The head of the captured output — the part that diagnoses.
    pub fn log_head(&self, worktree: &Path, bytes: usize) -> Option<String> {
        let text = std::fs::read_to_string(self.log_path(worktree)).ok()?;
        Some(match text.char_indices().nth(bytes) {
            Some((cut, _)) => format!("{}\n… ({} more bytes in the log)", &text[..cut], text.len() - cut),
            None => text,
        })
    }

    /// Read this checkout's recipe. `Ok(None)` = the repository has none, which
    /// is every repository until it writes one.
    pub fn recipe(&self, worktree: &Path) -> Result<Option<Recipe>, String> {
        let path = worktree.join(RECIPE_PATH);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("could not read {}: {e}", path.display())),
        };
        Recipe::parse(&text).map(Some)
    }

    /// Park the work that is waiting on this preparation, so that a retry
    /// which succeeds can release it without anybody minting a second attempt.
    ///
    /// Opaque to this module on purpose: it is the caller's payload (the
    /// board's brief, as a session command), and preparation has no business
    /// knowing what an attempt is.
    pub fn park(&self, worktree: &Path, payload: &str) -> std::io::Result<()> {
        let dir = self.state_dir(worktree);
        std::fs::create_dir_all(&dir)?;
        atomic_create(&self.brief_path(worktree), payload.as_bytes())
    }

    /// Read the parked payload without taking it — what a failure path uses to
    /// find out whom to tell, having decided not to release anything.
    pub fn parked(&self, worktree: &Path) -> Option<String> {
        std::fs::read_to_string(self.brief_path(worktree))
            .or_else(|_| std::fs::read_to_string(self.claimed_brief_path(worktree)))
            .ok()
    }

    /// Atomically claim the parked payload. The rename is the durable state
    /// transition; the in-process set excludes a second claimant while this
    /// process owns it. A claimed file left by a crash is recoverable because
    /// a fresh process starts with an empty ownership set.
    pub fn claim_parked(&self, worktree: &Path) -> Option<ParkedClaim> {
        let key = self.state_dir(worktree);
        let mut claims = self.claims.lock().unwrap_or_else(|e| e.into_inner());
        if !claims.insert(key.clone()) {
            return None;
        }
        let parked = self.brief_path(worktree);
        let claimed = self.claimed_brief_path(worktree);
        if parked.exists() && std::fs::rename(&parked, &claimed).is_err() {
            claims.remove(&key);
            return None;
        }
        if let Some(parent) = claimed.parent()
            && sync_directory(parent).is_err()
        {
            let _ = std::fs::rename(&claimed, &parked);
            claims.remove(&key);
            return None;
        }
        let payload = match std::fs::read_to_string(&claimed) {
            Ok(payload) => payload,
            Err(_) => {
                claims.remove(&key);
                return None;
            }
        };
        drop(claims);
        Some(ParkedClaim {
            prep: self.clone(),
            key,
            parked,
            claimed,
            payload,
            completed: false,
        })
    }

    /// Revoke work that has not been released yet. Cancellation calls this
    /// before interrupting the chat, so a late successful setup cannot start a
    /// run after its attempt was cancelled.
    pub fn revoke_parked(&self, worktree: &Path) {
        let parked = self.brief_path(worktree);
        let claimed = self.claimed_brief_path(worktree);
        let _ = std::fs::remove_file(&parked);
        let _ = std::fs::remove_file(claimed);
        if let Some(parent) = parked.parent() {
            let _ = sync_directory(parent);
        }
    }

    /// Cancel the active setup for this checkout, if there is one. The token
    /// reaches [`run_step`], which kills the entire process group.
    pub fn cancel(&self, worktree: &Path) -> bool {
        let key = canonical(worktree);
        let token = self
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .map(|(_, token)| token.clone());
        if let Some(token) = token {
            token.cancel();
            true
        } else {
            false
        }
    }

    /// Approve the exact recipe currently in `worktree` for this stable
    /// repository identity. The record lives outside the repository, so a
    /// checkout cannot approve itself; editing the recipe changes its digest
    /// and invalidates the approval automatically.
    pub fn approve(&self, worktree: &Path, repository_id: &str) -> Result<String, String> {
        let recipe_path = worktree.join(RECIPE_PATH);
        let digest = file_digest(&recipe_path)
            .ok_or_else(|| format!("could not read {}", recipe_path.display()))?;
        self.recipe(worktree)?
            .ok_or_else(|| format!("{} has no {RECIPE_PATH}", worktree.display()))?;
        let approval = Approval {
            repository_id: repository_id.to_string(),
            recipe_digest: digest.clone(),
            approved_at: now_ms(),
        };
        let text = serde_json::to_vec_pretty(&approval).map_err(|e| e.to_string())?;
        atomic_replace(&self.approval_path(repository_id, &digest), &text)
            .map_err(|e| e.to_string())?;
        Ok(digest)
    }

    fn approved(&self, repository_id: &str, digest: Option<&str>) -> bool {
        let Some(digest) = digest else { return false };
        let Ok(text) = std::fs::read_to_string(self.approval_path(repository_id, digest)) else {
            return false;
        };
        serde_json::from_str::<Approval>(&text).is_ok_and(|approval| {
            approval.repository_id == repository_id && approval.recipe_digest == digest
        })
    }

    /// Prepare the checkout: project the links, run `[setup]`, write down what
    /// happened. Never returns an error — the record is the answer.
    pub async fn prepare(&self, req: PrepareRequest<'_>) -> PrepRecord {
        let worktree = canonical(req.worktree);
        let recipe = match self.recipe(&worktree) {
            Ok(None) => {
                return PrepRecord::nothing_to_do(
                    &worktree,
                    &format!("no {RECIPE_PATH} in this checkout"),
                );
            }
            Ok(Some(recipe)) => recipe,
            Err(detail) => {
                // A recipe that does not parse is a failure and not an
                // absence: the repository asked for something and we cannot
                // tell what, which is the case where guessing is worst.
                return self.write(failed_record(&worktree, None, None, detail));
            }
        };
        let digest = file_digest(&worktree.join(RECIPE_PATH));

        if !req.force
            && let Some(previous) = self.status(&worktree)
            && previous.state == PrepState::Ready
            && previous.recipe_digest == digest
        {
            // Idempotent at the *outer* level too: the repository's script has
            // to be safe to re-run, and this is what means it usually is not
            // re-run at all. An edited recipe changes the digest and prepares
            // again with nobody having to remember.
            return previous;
        }

        // A retry may race a sync or another explicit retry. Only one command
        // tree may prepare a checkout; every other caller observes the same
        // persisted lifecycle instead of starting the script twice.
        let token = req.cancel.clone().unwrap_or_default();
        let generation = uuid::Uuid::new_v4().to_string();
        {
            let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
            if active.contains_key(&worktree) {
                return self.status(&worktree).unwrap_or_else(|| {
                    failed_record(
                        &worktree,
                        digest.clone(),
                        recipe.setup.as_ref().map(|step| step.run.clone()),
                        "preparation is already running for this checkout".to_string(),
                    )
                });
            }
            active.insert(worktree.clone(), (generation.clone(), token.clone()));
        }
        let _active = ActivePreparation {
            active: Arc::clone(&self.active),
            key: worktree.clone(),
            generation,
        };

        let run_command = recipe.run.as_ref().map(|s| s.run.clone());
        let command = recipe.setup.as_ref().map(|s| s.run.clone());
        let mut record = PrepRecord {
            state: PrepState::Preparing,
            worktree: worktree.to_string_lossy().into_owned(),
            recipe_digest: digest.clone(),
            command: command.clone(),
            started_at: now_ms(),
            ended_at: None,
            exit_code: None,
            detail: None,
            log: None,
            links: Vec::new(),
            run_command: run_command.clone(),
            requires_approval: false,
        };
        self.write(record.clone());

        // Both executable setup and machine-local projection cross the
        // repository boundary. Paths being constrained is necessary but not
        // sufficient: without this exact-digest approval, a repository edit
        // could name a different allowlisted credential leaf and make it
        // reachable to its eventual agent without the host agreeing.
        if recipe.requires_host_approval()
            && !self.approved(req.repository_id, digest.as_deref())
        {
            record.state = PrepState::Failed;
            record.ended_at = Some(now_ms());
            record.requires_approval = true;
            record.detail = Some(format!(
                "repository preparation is not approved on this host (recipe {})",
                digest.as_deref().unwrap_or("has no digest")
            ));
            return self.write(record);
        }

        // Links first: a setup script's whole reason for existing may be the
        // `.env.local` it reads.
        let locals = self.locals_root(req.repository_id);
        match project_links(&recipe.links, &locals, &worktree) {
            Ok(outcomes) => record.links = outcomes,
            Err(detail) => {
                record.state = PrepState::Failed;
                record.ended_at = Some(now_ms());
                record.detail = Some(detail);
                return self.write(record);
            }
        }

        let Some(step) = recipe.setup.as_ref() else {
            record.state = PrepState::Ready;
            record.ended_at = Some(now_ms());
            record.detail = Some(format!("{RECIPE_PATH} has no [setup] step"));
            return self.write(record);
        };

        let log = self.log_path(&worktree);
        record.log = Some(log.to_string_lossy().into_owned());
        let outcome = run_step(
            &step.run,
            &worktree,
            &recipe.env,
            recipe.setup_timeout(),
            &log,
            Some(&token),
        )
        .await;

        record.ended_at = Some(now_ms());
        record.exit_code = outcome.exit_code;
        match outcome.verdict {
            Verdict::Ok => {
                record.state = PrepState::Ready;
                record.detail = None;
            }
            Verdict::Failed(detail) => {
                record.state = PrepState::Failed;
                record.detail = Some(detail);
            }
        }
        self.write(record)
    }

    /// Run the recipe's `[archive]` step — bounded cleanup of reproducible
    /// output, before whatever generic sweep the caller does next.
    ///
    /// Best-effort by construction: the caller is reclaiming disk, and a repo
    /// whose `cargo clean` fails should still lose its `target/`. The returned
    /// string is what happened, for the caller's log; `None` = nothing to run.
    pub async fn archive(
        &self,
        worktree: &Path,
        repository_id: &str,
        cancel: Option<&CancellationToken>,
    ) -> Option<String> {
        let worktree = canonical(worktree);
        let recipe = self.recipe(&worktree).ok().flatten()?;
        let step = recipe.archive.as_ref()?;
        let digest = file_digest(&worktree.join(RECIPE_PATH));
        if !self.approved(repository_id, digest.as_deref()) {
            return Some(format!(
                "`{}` was not run because this recipe is not approved on this host",
                step.run
            ));
        }
        let log = self.state_dir(&worktree).join("archive.log");
        let outcome = run_step(
            &step.run,
            &worktree,
            &recipe.env,
            recipe.archive_timeout(),
            &log,
            cancel,
        )
        .await;
        Some(match outcome.verdict {
            Verdict::Ok => format!("`{}` succeeded", step.run),
            Verdict::Failed(detail) => format!("`{}` {detail}", step.run),
        })
    }

    /// Forget a checkout's preparation state. Called when the checkout itself
    /// is reclaimed — the record is about a directory, and outliving it by a
    /// week helps nobody.
    pub fn forget(&self, worktree: &Path) {
        let _ = std::fs::remove_dir_all(self.state_dir(worktree));
    }

    fn write(&self, record: PrepRecord) -> PrepRecord {
        let dir = self.state_dir(Path::new(&record.worktree));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(dir = %dir.display(), error = %e, "checkout prep state dir");
            return record;
        }
        match serde_json::to_string_pretty(&record) {
            Ok(text) => {
                if let Err(e) = atomic_replace(&dir.join("prep.json"), text.as_bytes()) {
                    tracing::warn!(dir = %dir.display(), error = %e, "writing checkout prep record");
                }
            }
            Err(e) => tracing::warn!(error = %e, "serializing checkout prep record"),
        }
        record
    }
}

fn failed_record(
    worktree: &Path,
    digest: Option<String>,
    command: Option<String>,
    detail: String,
) -> PrepRecord {
    PrepRecord {
        state: PrepState::Failed,
        worktree: worktree.to_string_lossy().into_owned(),
        recipe_digest: digest,
        command,
        started_at: now_ms(),
        ended_at: Some(now_ms()),
        exit_code: None,
        detail: Some(detail),
        log: None,
        links: Vec::new(),
        run_command: None,
        requires_approval: false,
    }
}

// ── links ───────────────────────────────────────────────────────────────────

/// Project each declared file from the box-local directory into the checkout.
///
/// Refusals, all of them decided before anything is opened:
/// - a `from` or `to` that is absolute or contains `..` (rejected at parse);
/// - a source that is not a *regular* file. A symlink in the locals directory
///   is refused rather than followed: following one would reintroduce exactly
///   the arbitrary-path reach the rooted `from` exists to prevent, one
///   `ln -s ~/.ssh/id_ed25519` at a time;
/// - a destination that already exists — kept, never clobbered, and said so in
///   the outcome. A checkout is somebody's working tree and this is not the
///   thing that overwrites their file.
///
/// A missing source is *not* a refusal. The operator has not filled that leaf
/// in yet, and turning that into a failed dispatch would make an optional
/// convenience a hard dependency of every attempt; it is recorded as `missing`,
/// which is a line somebody reads when the setup script then complains.
fn project_links(
    links: &[Link],
    locals: &Path,
    worktree: &Path,
) -> Result<Vec<LinkOutcome>, String> {
    let mut out = Vec::new();
    for link in links {
        let from_rel = relative_within("from", &link.from)?;
        let to_rel = relative_within("to", &link.to)?;
        let from = locals.join(&from_rel);
        let to = worktree.join(&to_rel);

        ensure_no_symlink_ancestors(locals, &from_rel, true)?;
        ensure_no_symlink_ancestors(worktree, &to_rel, false)?;
        let meta = match std::fs::symlink_metadata(&from) {
            Ok(meta) => meta,
            Err(_) => {
                out.push(LinkOutcome {
                    from: from.to_string_lossy().into_owned(),
                    to: link.to.clone(),
                    result: "missing".into(),
                    mode: None,
                });
                continue;
            }
        };
        if !meta.is_file() {
            return Err(format!(
                "{} is not a regular file — a link's source may not be a directory \
                 or a symlink, which would let it name a path outside {}",
                from.display(),
                locals.display()
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // A shared host may have files belonging to several people under
            // one administrator-managed tree. The engine projects only its
            // own user's files; copying another uid's credential into an
            // agent-owned checkout would silently change its ownership.
            // SAFETY: geteuid has no preconditions and only reads process state.
            let engine_uid = unsafe { libc::geteuid() };
            if meta.uid() != engine_uid {
                return Err(format!(
                    "{} is owned by uid {}, not the engine uid {}",
                    from.display(),
                    meta.uid(),
                    engine_uid
                ));
            }
        }
        if std::fs::symlink_metadata(&to).is_ok() {
            out.push(LinkOutcome {
                from: from.to_string_lossy().into_owned(),
                to: link.to.clone(),
                result: "kept".into(),
                mode: mode_of(&to),
            });
            continue;
        }
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("creating {} for a link: {e}", parent.display()))?;
        }
        let mut source = std::fs::File::open(&from)
            .map_err(|e| format!("opening {} for a link: {e}", from.display()))?;
        let mut destination = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&to)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                out.push(LinkOutcome {
                    from: from.to_string_lossy().into_owned(),
                    to: link.to.clone(),
                    result: "kept".into(),
                    mode: mode_of(&to),
                });
                continue;
            }
            Err(e) => return Err(format!("creating {} for a link: {e}", to.display())),
        };
        std::io::copy(&mut source, &mut destination)
            .map_err(|e| format!("copying {} into the checkout: {e}", from.display()))?;
        // Copy, not symlink, and the mode carried over: a symlink out of the
        // checkout is unreadable to a sandboxed run and editable back into the
        // operator's own file by an unsandboxed one, and a credential that
        // arrives world-readable is a finding whichever way it got there.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = meta.permissions().mode();
            let _ = std::fs::set_permissions(&to, std::fs::Permissions::from_mode(mode));
        }
        out.push(LinkOutcome {
            from: from.to_string_lossy().into_owned(),
            to: link.to.clone(),
            result: "copied".into(),
            mode: mode_of(&to),
        });
    }
    Ok(out)
}

/// Refuse every existing symlink from `root` down to the requested leaf.
/// Checking only the leaf is insufficient: either a source or destination
/// parent can redirect a relative path outside its promised root.
fn ensure_no_symlink_ancestors(
    root: &Path,
    relative: &Path,
    include_leaf: bool,
) -> Result<(), String> {
    let root_meta = match std::fs::symlink_metadata(root) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("could not inspect projection root {}: {e}", root.display())),
    };
    if root_meta.file_type().is_symlink() {
        return Err(format!("projection root {} is a symlink", root.display()));
    }
    let components: Vec<_> = relative.components().collect();
    let limit = if include_leaf {
        components.len()
    } else {
        components.len().saturating_sub(1)
    };
    let mut cursor = root.to_path_buf();
    for component in components.into_iter().take(limit) {
        cursor.push(component.as_os_str());
        match std::fs::symlink_metadata(&cursor) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!(
                    "{} is a symlink — projected files may not cross a symlink ancestor",
                    cursor.display()
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => return Err(format!("could not inspect {}: {e}", cursor.display())),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn mode_of(path: &Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).ok()?;
    Some(format!("{:04o}", meta.permissions().mode() & 0o7777))
}

#[cfg(not(unix))]
fn mode_of(_path: &Path) -> Option<String> {
    None
}

// ── running a step ──────────────────────────────────────────────────────────

enum Verdict {
    Ok,
    Failed(String),
}

struct StepOutcome {
    verdict: Verdict,
    exit_code: Option<i32>,
}

/// Run one recipe step from the worktree root, capped in time and in output.
///
/// The child gets its own process group so that the timeout and the
/// cancellation kill the *tree*: killing the `sh` alone leaves the `npm` it
/// started running, which is how a bounded setup becomes an unbounded one that
/// also lies about having stopped.
async fn run_step(
    command: &str,
    worktree: &Path,
    env: &BTreeMap<String, String>,
    timeout: Duration,
    log_path: &Path,
    cancel: Option<&CancellationToken>,
) -> StepOutcome {
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(worktree)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The engine process may hold board/GitHub credentials. A repository
        // command receives a useful shell/toolchain baseline, never the
        // engine's ambient environment wholesale.
        .env_clear()
        // What a setup script is allowed to know about where it is. Named
        // rather than left to the script's own `pwd` so a repo whose setup is a
        // shared script across several checkouts can tell them apart.
        .env("COMET_WORKTREE", worktree)
        .env("COMET_PREPARE", "1")
        .kill_on_drop(true);
    for name in [
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "PATH",
        "TMPDIR",
        "TEMP",
        "TMP",
        "LANG",
        "TERM",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "NVM_DIR",
        "VOLTA_HOME",
        "PNPM_HOME",
        "BUN_INSTALL",
    ] {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(name, value);
        }
    }
    for (name, value) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("LC_") {
            cmd.env(name, value);
        }
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return StepOutcome {
                verdict: Verdict::Failed(format!("could not start `{command}`: {e}")),
                exit_code: None,
            };
        }
    };
    let pid = child.id().map(|p| p as i32);
    let mut group = ProcessGroupGuard(pid);
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();

    // One file, both streams, interleaved as they arrive — a setup log is read
    // for "what went wrong", and errors that arrived in a separate file from
    // the command that caused them answer that badly.
    let mut sink = LogSink::create(log_path, command);
    let pump = async {
        let mut out_buf = [0u8; 8192];
        let mut err_buf = [0u8; 8192];
        loop {
            let (out_done, err_done) = (stdout.is_none(), stderr.is_none());
            if out_done && err_done {
                return;
            }
            tokio::select! {
                n = async { stdout.as_mut().unwrap().read(&mut out_buf).await }, if !out_done => {
                    match n {
                        Ok(0) | Err(_) => stdout = None,
                        Ok(n) => sink.write(&out_buf[..n]),
                    }
                }
                n = async { stderr.as_mut().unwrap().read(&mut err_buf).await }, if !err_done => {
                    match n {
                        Ok(0) | Err(_) => stderr = None,
                        Ok(n) => sink.write(&err_buf[..n]),
                    }
                }
            }
        }
    };

    let wait = async {
        pump.await;
        child.wait().await
    };
    let cancelled = async {
        match cancel {
            Some(token) => token.cancelled().await,
            None => std::future::pending().await,
        }
    };

    let (outcome, completed) = tokio::select! {
        status = wait => (match status {
            Ok(status) => {
                let code = status.code();
                if status.success() {
                    StepOutcome { verdict: Verdict::Ok, exit_code: code }
                } else {
                    StepOutcome {
                        verdict: Verdict::Failed(match code {
                            Some(code) => format!("exited {code}"),
                            None => "was killed by a signal".to_string(),
                        }),
                        exit_code: code,
                    }
                }
            }
            Err(e) => StepOutcome {
                verdict: Verdict::Failed(format!("could not be waited on: {e}")),
                exit_code: None,
            },
        }, true),
        _ = tokio::time::sleep(timeout) => {
            kill_group(pid);
            (StepOutcome {
                verdict: Verdict::Failed(format!("ran past its {} budget", format_duration(timeout))),
                exit_code: None,
            }, false)
        }
        _ = cancelled => {
            kill_group(pid);
            (StepOutcome {
                verdict: Verdict::Failed("was cancelled".to_string()),
                exit_code: None,
            }, false)
        }
    };
    if completed {
        group.0 = None;
    }
    sink.finish(&outcome.verdict);
    outcome
}

/// Last-resort cleanup when the async preparation future itself is dropped
/// (for example an RPC request is cancelled). `Child::kill_on_drop` kills only
/// the shell; this guard owns the process-group promise.
struct ProcessGroupGuard(Option<i32>);

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        kill_group(self.0);
    }
}

/// SIGKILL the child's whole process group. `-pid` is the group, which is the
/// child's own because [`run_step`] put it in one.
fn kill_group(pid: Option<i32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        // SAFETY: a plain kill(2) on a group this process created. A pid that
        // has already been reaped answers ESRCH, which is the no-op we want.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// The capped output file. Keeps the head and says how much it dropped —
/// see [`LOG_CAP_BYTES`] for why the head.
struct LogSink {
    file: Option<std::fs::File>,
    written: usize,
    dropped: usize,
}

impl LogSink {
    fn create(path: &Path, command: &str) -> LogSink {
        let mut sink = LogSink {
            file: std::fs::File::create(path).ok(),
            written: 0,
            dropped: 0,
        };
        // The command at the top: a log whose first line is `error: no such
        // file` is a log you cannot act on without going to find the recipe.
        sink.write(format!("$ {command}\n").as_bytes());
        sink
    }

    fn write(&mut self, bytes: &[u8]) {
        use std::io::Write;
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let room = LOG_CAP_BYTES.saturating_sub(self.written);
        if room == 0 {
            self.dropped += bytes.len();
            return;
        }
        let take = room.min(bytes.len());
        if file.write_all(&bytes[..take]).is_ok() {
            self.written += take;
        }
        self.dropped += bytes.len() - take;
    }

    fn finish(&mut self, verdict: &Verdict) {
        use std::io::Write;
        let dropped = self.dropped;
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if dropped > 0 {
            let _ = write!(
                file,
                "\n[comet] {dropped} further bytes were dropped — the head is kept \
                 because the first error is what diagnoses a setup\n"
            );
        }
        let _ = match verdict {
            Verdict::Ok => writeln!(file, "\n[comet] setup succeeded"),
            Verdict::Failed(detail) => writeln!(file, "\n[comet] setup {detail}"),
        };
        let _ = file.flush();
    }
}

// ── small shared helpers ────────────────────────────────────────────────────

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn identity_key(repository_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(repository_id.as_bytes());
    hex(&hasher.finalize())[..24].to_string()
}

/// Create a durable file without replacing an existing one. `rename` is not
/// suitable here because Unix rename replaces; a hard link publishes the
/// fully-written temp file atomically and returns AlreadyExists to the loser.
fn atomic_create(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent")
    })?;
    std::fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|name| name.to_str()).unwrap_or("state"),
        uuid::Uuid::new_v4()
    ));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::hard_link(&temp, path)?;
        sync_directory(parent)
    })();
    let _ = std::fs::remove_file(&temp);
    result
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent")
    })?;
    std::fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|name| name.to_str()).unwrap_or("state"),
        uuid::Uuid::new_v4()
    ));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        sync_directory(parent)
    })();
    let _ = std::fs::remove_file(&temp);
    result
}

/// Persist a state-file create, rename, or removal rather than only its bytes.
/// Directory syncing is meaningful on Unix; other platforms keep the same
/// atomic operations and rely on their filesystem's rename durability.
fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn file_digest(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Some(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recipe(text: &str) -> Result<Recipe, String> {
        Recipe::parse(text)
    }

    #[test]
    fn a_full_recipe_parses() {
        let r = recipe(
            r#"
version = 1
[setup]
run = "scripts/setup.sh"
timeout = "20m"
[run]
run = "cargo run"
[archive]
run = "cargo clean"
[env]
RUST_BACKTRACE = "1"
[[link]]
from = "dev.env"
to = ".env.local"
"#,
        )
        .expect("parses");
        assert_eq!(r.setup.as_ref().unwrap().run, "scripts/setup.sh");
        assert_eq!(r.setup_timeout(), Duration::from_secs(1200));
        assert_eq!(r.run.as_ref().unwrap().run, "cargo run");
        assert_eq!(r.archive_timeout(), DEFAULT_ARCHIVE_TIMEOUT);
        assert_eq!(r.env.get("RUST_BACKTRACE").unwrap(), "1");
        assert_eq!(r.links.len(), 1);
        assert!(r.requires_host_approval());
    }

    #[test]
    fn an_empty_recipe_is_a_recipe() {
        let r = recipe("version = 1").expect("parses");
        assert!(r.setup.is_none());
        assert_eq!(r.setup_timeout(), DEFAULT_SETUP_TIMEOUT);
    }

    #[test]
    fn an_explicit_run_is_an_offer_while_host_effects_require_approval() {
        assert!(!recipe("version = 1\n[run]\nrun = \"cargo run\"\n")
            .unwrap()
            .requires_host_approval());
        assert!(recipe("version = 1\n[archive]\nrun = \"cargo clean\"\n")
            .unwrap()
            .requires_host_approval());
    }

    #[test]
    fn a_misspelled_key_is_refused_rather_than_ignored() {
        // The whole point: `timout = "20m"` silently meaning "10m" is the
        // class of failure this file exists to remove.
        let err = recipe("version = 1\n[setup]\nrun = \"x\"\ntimout = \"20m\"\n").unwrap_err();
        assert!(err.contains("timout"), "{err}");
    }

    #[test]
    fn another_schema_version_is_refused_by_name() {
        let err = recipe("version = 2\n").unwrap_err();
        assert!(err.contains("version = 2") && err.contains("version 1"), "{err}");
    }

    #[test]
    fn a_timeout_past_the_ceiling_is_refused_not_clamped() {
        let err = recipe("version = 1\n[setup]\nrun = \"x\"\ntimeout = \"2h\"\n").unwrap_err();
        assert!(err.contains("2h") && err.contains("1h"), "{err}");
    }

    #[test]
    fn an_unparseable_timeout_names_the_step() {
        let err = recipe("version = 1\n[archive]\nrun = \"x\"\ntimeout = \"soon\"\n").unwrap_err();
        assert!(err.contains("[archive]") && err.contains("soon"), "{err}");
    }

    #[test]
    fn a_recipe_may_not_move_comets_own_state() {
        // A file in a pull request must not be able to point the agent's
        // `comet-board` at a different board.
        let state_dir = ["COMET", "BOARD", "STATE", "DIR"].join("_");
        let err = recipe(&format!(
            "version = 1\n[env]\n{state_dir} = \"/tmp/evil\"\n"
        ))
        .unwrap_err();
        assert!(err.contains(&state_dir), "{err}");
        for name in ["PATH", "LD_PRELOAD", "GIT_ASKPASS", "DYLD_INSERT_LIBRARIES"] {
            let err = recipe(&format!("version = 1\n[env]\n{name} = \"x\"\n")).unwrap_err();
            assert!(err.contains(name), "{name} should be refused: {err}");
        }
        // A name that merely starts with the same letters is not reserved.
        assert!(recipe("version = 1\n[env]\nCOMETARY = \"1\"\n").is_ok());
    }

    #[test]
    fn a_link_may_not_spell_a_path_out_of_its_root() {
        for from in ["/etc/passwd", "../../.ssh/id_ed25519", "a/../../b"] {
            let err =
                recipe(&format!("version = 1\n[[link]]\nfrom = \"{from}\"\nto = \"x\"\n")).unwrap_err();
            assert!(err.contains(from), "{from} should be refused: {err}");
        }
        for to in ["/etc/cron.d/x", "../outside"] {
            let err = recipe(&format!(
                "version = 1\n[[link]]\nfrom = \"a\"\nto = \"{to}\"\n"
            ))
            .unwrap_err();
            assert!(err.contains(to), "{to} should be refused: {err}");
        }
    }

    #[test]
    fn the_mcp_table_is_carried_and_not_acted_on() {
        // gh#273's seam: a repo that fills it in early must not be a parse
        // error on an engine that predates the feature.
        let r = recipe("version = 1\n[mcp]\nservers = { linear = \"npx -y linear-mcp\" }\n")
            .expect("parses");
        assert!(r.mcp.is_some());
    }

    #[test]
    fn durations_read_the_way_a_recipe_writes_them() {
        assert_eq!(parse_duration("90s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("10m"), Some(Duration::from_secs(600)));
        assert_eq!(parse_duration("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_duration("45"), Some(Duration::from_secs(45)));
        assert_eq!(parse_duration("later"), None);
        assert_eq!(format_duration(Duration::from_secs(600)), "10m");
        assert_eq!(format_duration(Duration::from_secs(3600)), "1h");
        assert_eq!(format_duration(Duration::from_secs(90)), "90s");
    }

    #[test]
    fn a_locals_root_is_per_repository_and_under_the_data_dir() {
        let prep = CheckoutPrep::new(Path::new("/data"));
        let a = prep.locals_root("device:/home/x/src/comet-board/.git");
        let b = prep.locals_root("device:/home/y/src/comet-board/.git");
        assert!(a.starts_with("/data/locals"));
        assert_ne!(a, b, "same basename is not a repository identity");
    }

    #[test]
    fn state_dirs_are_per_checkout_and_flat() {
        let prep = CheckoutPrep::new(Path::new("/data"));
        let a = prep.state_dir(Path::new("/w/board-gh-1"));
        let b = prep.state_dir(Path::new("/w/board-gh-2"));
        assert_ne!(a, b);
        assert_eq!(a.parent(), b.parent());
        assert!(a.starts_with("/data/checkout-prep"));
    }

    #[test]
    fn the_log_sink_keeps_the_head_and_says_what_it_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prep.log");
        let mut sink = LogSink::create(&path, "true");
        sink.write(&vec![b'x'; LOG_CAP_BYTES * 2]);
        sink.finish(&Verdict::Ok);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("$ true\n"));
        assert!(text.contains("further bytes were dropped"), "{}", &text[..200]);
        assert!(text.len() < LOG_CAP_BYTES + 1024);
    }
}
