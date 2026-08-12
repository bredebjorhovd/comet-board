//! Codex harness: spawns the installed `codex` CLI as `codex app-server` and
//! speaks JSON-RPC 2.0 over stdio — the same interface the Codex IDE extension
//! uses (spec: docs/research/harness.md; behavior ported from comet's
//! `packages/harness/src/codex.ts`).
//!
//! - `initialize` handshake (clientInfo + `capabilities.experimentalApi`) then
//!   the `initialized` notification; unknown notification methods tolerated.
//! - `thread/start` (or `thread/resume` with a fresh-start fallback) →
//!   `SessionStarted`; `turn/start` carries the prompt, model, effort,
//!   `sandboxPolicy`, and approval policy.
//! - Notifications map to [`AgentEvent`]s: agentMessage/reasoning deltas (both
//!   `delta`/`textDelta` spellings), item lifecycles → typed ToolCall/ToolResult,
//!   `thread/tokenUsage/updated` → Usage, turn/completed|failed|aborted → Done.
//! - Approvals: the wire policy is always `"never"` — parity with the Claude
//!   adapter's auto-approve-everything (unattended runs; the sandbox is the
//!   guardrail). Stray `item/commandExecution/requestApproval` +
//!   `item/fileChange/requestApproval` still round-trip through
//!   [`RunControls::request_input`] as a synthesized yes/no question.
//! - Steering: `turn/steer { expectedTurnId }` into the live turn; a rejected
//!   steer (the turn-completed race) is queued and delivered as the next
//!   `turn/start` on the same thread. The session is persistent across turns
//!   while the steering mailbox lives.
//! - Interrupt: cancelling [`RunControls::interrupt`] sends `turn/interrupt`,
//!   escalating to SIGTERM → SIGKILL if the child is unresponsive; the stream
//!   always ends with `Done { status: Interrupted }`.

mod catalog;
mod normalize;

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use comet_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode,
    UserInputAnswer, UserInputQuestion,
};

use crate::jsonrpc::{Incoming, RpcClient};
use crate::{Harness, HarnessError, RunControls};
use catalog::{REASONING_LEVELS, sandbox_mode, sandbox_policy_value, static_models, to_effort};
use normalize::{
    Phase, context_event, delta_text, item_id, item_type, map_item, turn_error_message, turn_id,
    usage_event,
};

/// Locate the device's installed Codex CLI: `CODEX_EXECUTABLE`, then PATH, then
/// common install locations GUI launches miss. Resolved per call — cheap, and
/// PATH may be adopted from the login shell after startup.
pub(crate) fn resolve_codex_executable() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("CODEX_EXECUTABLE")
        && !p.is_empty()
    {
        return Some(PathBuf::from(p));
    }
    let exe = if cfg!(windows) { "codex.exe" } else { "codex" };
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.join(exe))
                .collect()
        })
        .unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        candidates.push(home.join(".local").join("bin").join("codex"));
        candidates.push(home.join(".codex").join("bin").join("codex"));
        candidates.push(home.join(".npm-global").join("bin").join("codex"));
    }
    candidates.push(PathBuf::from("/opt/homebrew/bin/codex"));
    candidates.push(PathBuf::from("/usr/local/bin/codex"));
    candidates.extend(
        crate::node_version_manager_bins()
            .into_iter()
            .map(|d| d.join(exe)),
    );
    candidates.into_iter().find(|p| p.exists())
}

/// The first codex-cli this workspace has verified the worktree-mount fix in
/// (§gh#349). Below it, [`worktree_on_slashed_branch`] still earns the
/// escalation; at or above it, the escalation is a sandbox dropped for a bug
/// that is not there.
///
/// 0.144.x is the last release verified *broken* and 0.147.0 the first
/// verified *fixed*, so 0.145 and 0.146 sit in a band nobody measured. The
/// threshold sits at the top of that band on purpose: being wrong here costs a
/// visible escalation on two patch releases, and being wrong the other way
/// costs a run where no command can start.
const WORKTREE_MOUNT_FIXED_IN: CliVersion = (0, 147, 0);

/// True when `cwd` is a LINKED git worktree whose checked-out branch name
/// contains '/' — the exact shape that trips codex's sandbox worktree-mount
/// derivation (see the escalation in [`CodexHarness::run`]). Pure filesystem
/// reads: the worktree's `.git` pointer FILE names the admin dir, whose HEAD
/// symref carries the branch.
fn worktree_on_slashed_branch(cwd: &str) -> bool {
    if cwd.is_empty() {
        return false;
    }
    let dot_git = std::path::Path::new(cwd).join(".git");
    let is_pointer_file = std::fs::metadata(&dot_git).is_ok_and(|m| m.is_file());
    if !is_pointer_file {
        return false; // main checkout (`.git` dir) — codex handles it fine
    }
    let Ok(pointer) = std::fs::read_to_string(&dot_git) else {
        return false;
    };
    let Some(gitdir) = pointer.strip_prefix("gitdir:").map(str::trim) else {
        return false;
    };
    let Ok(head) = std::fs::read_to_string(std::path::Path::new(gitdir).join("HEAD")) else {
        return false;
    };
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .is_some_and(|branch| branch.contains('/'))
}

/// Where `cwd`'s git metadata actually lives — its git dir, and the common dir
/// it shares with the rest of the repository (§gh#349).
///
/// These are the paths codex's `workspace-write` sandbox has to be told about
/// or a dispatched agent cannot commit. Verified against codex-cli 0.147.0: the
/// sandbox denies writes to **exactly `<cwd>/.git`** — not a nested `.git`, not
/// another repository's, not a `.git`-prefixed sibling — and denies nothing
/// else about the workspace. So:
///
/// - A **main checkout** keeps its git dir at `<cwd>/.git`, squarely under the
///   deny rule. `git commit` fails on the index, `git fetch` fails on
///   `FETCH_HEAD`, and the agent is left able to edit files and unable to
///   record the edit — the report behind gh#349, verbatim.
/// - A **linked worktree** has a `.git` pointer *file* there instead, and its
///   real admin dir lives under the main checkout's `.git/worktrees/…`, outside
///   the workspace entirely. Nothing it writes is ever under the deny rule, so
///   it works — by accident, and only because the paths happen not to overlap.
///
/// Both are returned, existing paths only. The common dir is where a fetch puts
/// objects and remote refs, and in a worktree that is a different directory
/// from the git dir; a list that named only one of them would fix `commit` and
/// leave `pull` broken.
///
/// Pure filesystem reads (no `git` subprocess): the pointer file names the
/// admin dir, and the admin dir's `commondir` names the shared one, relative to
/// itself.
fn git_writable_roots(cwd: &str) -> Vec<PathBuf> {
    if cwd.is_empty() {
        return Vec::new();
    }
    let dot_git = std::path::Path::new(cwd).join(".git");
    let Ok(meta) = std::fs::metadata(&dot_git) else {
        return Vec::new(); // not a checkout at all
    };
    let git_dir = if meta.is_dir() {
        dot_git
    } else {
        // A worktree pointer: `gitdir: /abs/path/to/.git/worktrees/name`.
        let Some(target) = std::fs::read_to_string(&dot_git)
            .ok()
            .and_then(|p| p.strip_prefix("gitdir:").map(|t| PathBuf::from(t.trim())))
        else {
            return Vec::new();
        };
        target
    };
    let mut roots = vec![git_dir.clone()];
    // `commondir` is written relative to the admin dir (`../..`), so it is
    // joined and then normalized — a literal `…/worktrees/x/../..` would be a
    // path the sandbox matches against textually.
    if let Ok(common) = std::fs::read_to_string(git_dir.join("commondir")) {
        let common = git_dir.join(common.trim());
        if let Ok(common) = std::fs::canonicalize(&common) {
            roots.push(common);
        }
    }
    roots.retain(|p| p.is_absolute() && p.exists());
    roots.dedup();
    roots
}

/// A codex-cli version, `(major, minor, patch)`, compared against
/// [`WORKTREE_MOUNT_FIXED_IN`].
type CliVersion = (u32, u32, u32);

/// The installed CLI's version — `codex --version` prints `codex-cli 0.147.0`
/// (§gh#349).
///
/// `None` when the binary cannot be run or says something this cannot parse,
/// and every caller must read that as "no evidence", never as "old". The only
/// thing the version gates is a *sandbox drop*, and a drop is an exception that
/// has to be earned by evidence — see [`CodexHarness::run`].
///
/// Cached per executable path: `run` is called once per dispatch and a CLI does
/// not change version under a running box, but it can change between them, so
/// the cache is keyed by path rather than global.
fn codex_version(exe: &std::path::Path) -> Option<CliVersion> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Option<CliVersion>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Ok(map) = cache.lock()
        && let Some(hit) = map.get(exe)
    {
        return *hit;
    }
    let probed = std::process::Command::new(exe)
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| parse_codex_version(&String::from_utf8_lossy(&out.stdout)));
    if let Ok(mut map) = cache.lock() {
        map.insert(exe.to_path_buf(), probed);
    }
    probed
}

/// Pull `(major, minor, patch)` out of `codex-cli 0.147.0`. Tolerates a bare
/// `0.147.0`, a `v` prefix, and trailing pre-release junk; rejects anything
/// without three numbers, because a half-read version is worse than none.
fn parse_codex_version(out: &str) -> Option<CliVersion> {
    let token = out
        .split_whitespace()
        .find_map(|t| {
            let t = t.trim_start_matches('v');
            t.starts_with(|c: char| c.is_ascii_digit()).then_some(t)
        })?
        .trim_start_matches('v');
    let mut parts = token.split(['.', '-', '+']).map(|p| p.parse::<u32>().ok());
    Some((parts.next()??, parts.next()??, parts.next()??))
}

/// The Codex harness. Construct with [`CodexHarness::new`]; tests point it at a
/// fake app server with [`CodexHarness::with_executable`].
pub struct CodexHarness {
    executable: Option<PathBuf>,
    /// Grace between `turn/interrupt` and SIGTERM.
    interrupt_grace: Duration,
    /// Grace between SIGTERM and SIGKILL.
    kill_grace: Duration,
}

impl Default for CodexHarness {
    fn default() -> Self {
        Self {
            executable: None,
            interrupt_grace: Duration::from_secs(2),
            kill_grace: Duration::from_secs(3),
        }
    }
}

impl CodexHarness {
    pub fn new() -> Self {
        Self::default()
    }

    /// Use a fixed CLI binary instead of PATH/known-location resolution.
    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        self
    }

    /// Tune the interrupt→SIGTERM→SIGKILL escalation timing.
    pub fn with_graces(mut self, interrupt_grace: Duration, kill_grace: Duration) -> Self {
        self.interrupt_grace = interrupt_grace;
        self.kill_grace = kill_grace;
        self
    }

    fn resolve_executable(&self) -> Result<PathBuf, HarnessError> {
        if let Some(p) = &self.executable {
            return Ok(p.clone());
        }
        resolve_codex_executable().ok_or_else(|| {
            HarnessError::NotInstalled(
                "codex (searched PATH, ~/.local/bin, ~/.codex/bin, ~/.npm-global/bin, \
                 /opt/homebrew/bin, /usr/local/bin, and fnm/nvm/volta/pnpm/bun install \
                 dirs; set CODEX_EXECUTABLE to override)"
                    .into(),
            )
        })
    }
}

#[async_trait]
impl Harness for CodexHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Codex
    }
    fn display_name(&self) -> &str {
        // "Codex" (not "Codex CLI") — comet composer/defaults.ts
        // HARNESS_LABEL; must also match the registry's lazy descriptor so
        // the catalog entry doesn't change after the first resolve.
        "Codex"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    /// Native `turn/steer` injects into the active turn; a steer that misses
    /// the turn falls back to a follow-up `turn/start` on the same thread.
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        REASONING_LEVELS
    }

    /// The curated static catalog (see [`catalog`]); requires an installed CLI
    /// so an absent binary surfaces as [`HarnessError::NotInstalled`] here.
    /// This is the seam for live discovery: a short-lived `codex app-server`
    /// paging `model/list` (experimentalApi) exactly as codex.ts does.
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        self.resolve_executable()?;
        Ok(static_models())
    }

    async fn run(
        &self,
        mut request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let exe = self.resolve_executable()?;
        // The two things this run has to settle about its own sandbox, and
        // then say out loud (§gh#349).
        //
        // **The git dir has to be writable.** Codex's `workspace-write` denies
        // writes to `<cwd>/.git` and nothing else, which leaves a dispatched
        // agent able to edit files and unable to commit them — see
        // [`git_writable_roots`], which is also the fix: naming those paths as
        // writable roots lifts the deny with the sandbox still on. That is
        // strictly better than what this code used to do, which was turn the
        // sandbox off.
        //
        // **The old escalation is now version-gated.** Codex ≤0.144.x derived
        // a MALFORMED worktree mount when the branch name contained '/'
        // (verified against the real CLI: `wing/x` in a linked worktree killed
        // every command before it started; `wing-x` was fine; full access
        // worked), so the adapter widened that shape to `danger-full-access`
        // rather than ship a session where nothing runs. Re-tested against
        // 0.147.0 and the bug is gone — a linked worktree on `board/gh-349-…`
        // runs, writes, and commits under plain `workspace-write`.
        //
        // Which matters more than a tidied workaround, because the board's
        // default branch template is `board/{identifier_lower}`: the
        // escalation fired on *every* board dispatch into a worktree, and said
        // so only in the WARN below. Gated on the version, it now fires on no
        // current box — and when it does fire, [`SandboxReport`] carries the
        // fact out of the log and onto the review.
        let requested = request.sandbox;
        let mut sandbox_report = comet_proto::SandboxReport::as_requested(requested);
        if requested == comet_proto::SandboxLevel::WorkspaceWrite
            && worktree_on_slashed_branch(&request.cwd)
            && codex_version(&exe).is_some_and(|v| v < WORKTREE_MOUNT_FIXED_IN)
        {
            let reason = "this codex predates the worktree-mount fix, and under it a linked \
                          worktree on a slash-named branch cannot run any command at all";
            tracing::warn!(
                cwd = %request.cwd,
                "codex sandbox escalated to danger-full-access: {reason}"
            );
            request.sandbox = comet_proto::SandboxLevel::DangerFullAccess;
            sandbox_report =
                comet_proto::SandboxReport::widened(requested, request.sandbox, reason);
        }
        let writable_roots = if request.sandbox == comet_proto::SandboxLevel::WorkspaceWrite {
            git_writable_roots(&request.cwd)
        } else {
            Vec::new()
        };
        let mut cmd = Command::new(&exe);
        cmd.arg("app-server");
        crate::prepend_exe_dir_to_path(&mut cmd, &exe);
        if let Some(chat_id) = &controls.chat_id {
            cmd.env("COMET_BOARD_CHAT_ID", chat_id);
        }
        if let Some(account) = &controls.account {
            account.apply(&mut cmd, HarnessId::Codex);
        }
        // Before the push credentials, whose `gh` shim has to stay in front.
        crate::prepend_dirs_to_path(&mut cmd, &controls.bin_dirs);
        if let Some(push) = &controls.push {
            push.apply(&mut cmd);
        }
        if !request.cwd.is_empty() {
            cmd.current_dir(&request.cwd);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                HarnessError::NotInstalled(exe.display().to_string())
            } else {
                HarnessError::Io(e)
            }
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| HarnessError::Protocol("codex child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HarnessError::Protocol("codex child has no stdout".into()))?;
        let stderr_tail = crate::StderrTail::default();
        if let Some(stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "comet_harness::codex", "stderr: {line}");
                    tail.push(&line);
                }
            });
        }

        let (client, incoming) = RpcClient::new(stdin, stdout);
        let (event_tx, event_rx) = mpsc::channel::<Result<AgentEvent, HarnessError>>(256);
        tokio::spawn(run_session(Session {
            child,
            client,
            incoming,
            event_tx,
            controls,
            request,
            sandbox_report,
            writable_roots,
            interrupt_grace: self.interrupt_grace,
            kill_grace: self.kill_grace,
            stderr_tail,
        }));

        Ok(futures::stream::unfold(event_rx, |mut rx| async move {
            rx.recv().await.map(|ev| (ev, rx))
        })
        .boxed())
    }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

struct Session {
    child: Child,
    client: RpcClient,
    incoming: mpsc::Receiver<Incoming>,
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    controls: RunControls,
    request: RunRequest,
    /// What sandbox this run asked for and what it got (§gh#349) — settled at
    /// spawn, announced once before the first turn.
    sandbox_report: comet_proto::SandboxReport,
    /// Paths handed to `workspace-write` as writable roots: the checkout's git
    /// metadata, without which the agent cannot commit. Empty at every other
    /// level, where the question does not arise.
    writable_roots: Vec<PathBuf>,
    interrupt_grace: Duration,
    kill_grace: Duration,
    /// Rolling stderr tail for the crash message on an unexpected exit.
    stderr_tail: crate::StderrTail,
}

/// Turn-routing state (port of codex.ts's activeTurnId/completedTurnIds): the
/// `turn/start` response and the turn lifecycle notifications are separate
/// app-server messages that may arrive in either order — never revive a turn
/// that `turn/completed` already declared finished.
#[derive(Default)]
struct TurnRouter {
    active: Option<String>,
    completed: VecDeque<String>,
}

impl TurnRouter {
    fn is_completed(&self, id: &str) -> bool {
        self.completed.iter().any(|c| c == id)
    }

    fn note_started(&mut self, id: String) {
        if id.is_empty() || self.is_completed(&id) {
            return;
        }
        // A replacement `turn/started` is authoritative evidence that a stale
        // active turn is over, even if its completion notification was lost.
        if let Some(prev) = self.active.take()
            && prev != id
        {
            self.remember_completed(prev);
        }
        self.active = Some(id);
    }

    fn note_completed(&mut self, id: &str) {
        if id.is_empty() {
            return;
        }
        self.remember_completed(id.to_owned());
        if self.active.as_deref() == Some(id) {
            self.active = None;
        }
    }

    /// Adopt a turn id from a `turn/start` RESPONSE (the notification is
    /// allowed to beat it).
    fn adopt_started(&mut self, id: String) {
        self.active = (!id.is_empty() && !self.is_completed(&id)).then_some(id);
    }

    fn remember_completed(&mut self, id: String) {
        self.completed.push_back(id);
        // Bounded so a months-long persistent session can't grow it forever.
        while self.completed.len() > 32 {
            self.completed.pop_front();
        }
    }
}

fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Rotate the assistant message id; returns (previous, next).
fn rotate(id: &mut String) -> (String, String) {
    let prev = std::mem::replace(id, new_message_id());
    (prev, id.clone())
}

async fn send(tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>, ev: AgentEvent) -> bool {
    tx.send(Ok(ev)).await.is_ok()
}

/// `turn/start` and return the new turn id from the response.
async fn start_turn(client: &RpcClient, params: Value) -> Result<String, HarnessError> {
    let started = client.request("turn/start", params).await?;
    Ok(started["turn"]["id"].as_str().unwrap_or("").to_owned())
}

/// The per-run event loop: one task multiplexing app-server messages, the
/// steering mailbox, the interrupt token, and consumer liveness.
async fn run_session(session: Session) {
    let Session {
        mut child,
        client,
        mut incoming,
        event_tx,
        controls,
        request,
        sandbox_report,
        writable_roots,
        interrupt_grace,
        kill_grace,
        stderr_tail,
    } = session;
    let RunControls {
        request_input,
        mut steering,
        interrupt,
        chat_id: _,
        account: _,
        // All three are spent at spawn: the account picked the config dir, the
        // credentials and the tool directories are already on the child.
        push: _,
        bin_dirs: _,
    } = controls;
    let request_input = Arc::new(request_input);

    // ---- wire params ------------------------------------------------------
    // Parity with the Claude adapter, which auto-approves every `can_use_tool`
    // regardless of `auto_approve` (comet sessions run unattended; the sandbox
    // is the guardrail): never surface wire approvals. "on-request" turned
    // every command into a yes/no question (user report: "asking me for
    // approval at every step"). The approval-as-input plumbing below stays for
    // stray requests and a future explicit permission-mode setting.
    let approval_policy = "never";
    let effort = to_effort(request.reasoning);
    // Service tier rides thread-start and every turn (mirrors the Codex IDE
    // client). "default" means Standard — omit it entirely.
    let service_tier = request
        .model_options
        .get("serviceTier")
        .and_then(Value::as_str)
        .filter(|t| *t != "default")
        .map(str::to_owned);

    let start_params = {
        let mut p = serde_json::Map::new();
        p.insert("cwd".into(), Value::String(request.cwd.clone()));
        p.insert("approvalPolicy".into(), approval_policy.into());
        p.insert("sandbox".into(), sandbox_mode(request.sandbox).into());
        if let Some(model) = &request.model {
            p.insert("model".into(), Value::String(model.clone()));
        }
        if let Some(tier) = &service_tier {
            p.insert("serviceTier".into(), Value::String(tier.clone()));
        }
        p
    };

    // ---- handshake + thread + first turn (interruptible) ------------------
    let setup = async {
        client
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "comet-native",
                        "title": "Comet",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "capabilities": { "experimentalApi": true },
                }),
            )
            .await?;
        client.notify("initialized", None);

        let thread = if let Some(resume) = &request.resume {
            let mut p = start_params.clone();
            p.insert("threadId".into(), Value::String(resume.clone()));
            match client.request("thread/resume", Value::Object(p)).await {
                Ok(thread) => thread,
                // A missing/foreign rollout falls back to a fresh thread.
                Err(e) => {
                    tracing::debug!(
                        target: "comet_harness::codex",
                        "thread/resume failed (starting fresh): {e}"
                    );
                    client
                        .request("thread/start", Value::Object(start_params.clone()))
                        .await?
                }
            }
        } else {
            client
                .request("thread/start", Value::Object(start_params.clone()))
                .await?
        };
        let thread_id = thread["thread"]["id"].as_str().unwrap_or("").to_owned();
        Ok::<String, HarnessError>(thread_id)
    };
    let thread_id = tokio::select! {
        res = setup => match res {
            Ok(thread_id) => thread_id,
            Err(e) => {
                let _ = event_tx
                    .send(Ok(AgentEvent::Done {
                        status: DoneStatus::Errored,
                        result: None,
                        error: Some(e.to_string()),
                        session_id: None,
                    }))
                    .await;
                shutdown_child(&mut child, kill_grace).await;
                return;
            }
        },
        _ = interrupt.cancelled() => {
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: None,
                }))
                .await;
            shutdown_child(&mut child, kill_grace).await;
            return;
        }
    };

    let turn_params = |text: &str| -> Value {
        let mut p = serde_json::Map::new();
        p.insert("threadId".into(), Value::String(thread_id.clone()));
        p.insert("input".into(), json!([{ "type": "text", "text": text }]));
        p.insert("approvalPolicy".into(), approval_policy.into());
        p.insert(
            "sandboxPolicy".into(),
            sandbox_policy_value(request.sandbox, &writable_roots),
        );
        // Reasoning summaries stream (`item/reasoning/summaryTextDelta`) only
        // when asked for — without this codex "thinks" in silence for minutes:
        // nothing renders and the UI's 45s staleness gate flips Working off
        // (user report: "not streaming, doesn't say it's working").
        p.insert("summary".into(), "auto".into());
        if let Some(model) = &request.model {
            p.insert("model".into(), Value::String(model.clone()));
        }
        if let Some(effort) = effort {
            p.insert("effort".into(), effort.into());
        }
        if let Some(tier) = &service_tier {
            p.insert("serviceTier".into(), Value::String(tier.clone()));
        }
        Value::Object(p)
    };

    let mut assistant_message_id = new_message_id();
    // Before `SessionStarted`, so the very first thing a journal records about
    // a run is the terms it ran on (§gh#349).
    if !send(&event_tx, AgentEvent::Sandbox(sandbox_report)).await {
        shutdown_child(&mut child, kill_grace).await;
        return;
    }
    if !send(
        &event_tx,
        AgentEvent::SessionStarted {
            harness: HarnessId::Codex,
            model: request.model.clone().unwrap_or_default(),
            tools: Vec::new(),
            cwd: request.cwd.clone(),
            session_id: thread_id.clone(),
            assistant_message_id: assistant_message_id.clone(),
        },
    )
    .await
    {
        shutdown_child(&mut child, kill_grace).await;
        return;
    }

    let mut router = TurnRouter::default();
    match start_turn(&client, turn_params(&request.prompt)).await {
        Ok(id) => router.adopt_started(id),
        Err(e) => {
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(e.to_string()),
                    session_id: Some(thread_id.clone()),
                }))
                .await;
            shutdown_child(&mut child, kill_grace).await;
            return;
        }
    }

    // ---- main loop --------------------------------------------------------
    // Deltas seen per agent-message item, so a model that never streams
    // (item/completed only) still emits its text exactly once.
    let mut streamed_text: HashSet<String> = HashSet::new();
    // Token usage is held until the turn ends, emitted just before Done.
    let mut pending_usage: Option<AgentEvent> = None;
    // …and context fullness is not held at all (gh#271): it is a level to
    // watch on a live attempt. Kept only to stay quiet when it has not moved.
    let mut last_context: Option<comet_proto::ContextUsage> = None;
    // Steers whose `turn/steer` lost the turn-completed race; delivered as the
    // next `turn/start` when the expected turn's end notification arrives.
    let mut queued_steers: VecDeque<String> = VecDeque::new();
    let mut steering_open = true;
    let mut interrupted = false;
    let mut interrupt_sent = false;
    // A Done has been emitted for the turn currently/last in flight.
    let mut done_current = false;
    let mut done_after_interrupt = false;
    let mut escalation: Option<tokio::task::JoinHandle<()>> = None;

    'main: loop {
        tokio::select! {
            inc = incoming.recv() => match inc {
                Some(Incoming::Notification { method, params }) => match method.as_str() {
                    "turn/started" => router.note_started(turn_id(&params)),

                    "item/agentMessage/delta" => {
                        streamed_text.insert(item_id(&params));
                        if let Some(text) = delta_text(&params)
                            && !send(&event_tx, AgentEvent::TextDelta { text }).await
                        {
                            break 'main;
                        }
                    }

                    "item/reasoning/textDelta" | "item/reasoning/summaryTextDelta" => {
                        if let Some(text) = delta_text(&params)
                            && !send(&event_tx, AgentEvent::ReasoningDelta { text }).await
                        {
                            break 'main;
                        }
                    }

                    "item/started" | "item/completed" => {
                        let phase = if method == "item/started" {
                            Phase::Started
                        } else {
                            Phase::Completed
                        };
                        let item = params.get("item").cloned().unwrap_or(Value::Null);
                        if matches!(item_type(&item), "agentMessage" | "agent_message") {
                            if phase == Phase::Completed {
                                // Fallback for non-streamed messages only.
                                let id = item.get("id").and_then(Value::as_str).unwrap_or("");
                                let text = item.get("text").and_then(Value::as_str).unwrap_or("");
                                if !streamed_text.contains(id)
                                    && !text.is_empty()
                                    && !send(&event_tx, AgentEvent::TextDelta { text: text.into() }).await
                                {
                                    break 'main;
                                }
                                // Deltas are token chunks, not steering
                                // boundaries: the completed item is the
                                // provider-authoritative end of the text part.
                                let (prev, _next) = rotate(&mut assistant_message_id);
                                if !send(
                                    &event_tx,
                                    AgentEvent::AssistantMessageCompleted {
                                        assistant_message_id: prev,
                                    },
                                )
                                .await
                                {
                                    break 'main;
                                }
                            }
                        } else {
                            for ev in map_item(phase, &item) {
                                if !send(&event_tx, ev).await {
                                    break 'main;
                                }
                            }
                        }
                    }

                    "thread/tokenUsage/updated" => {
                        if let Some(usage) = usage_event(&params) {
                            pending_usage = Some(usage);
                        }
                        // Fullness, unlike spend, is worth having *before* the
                        // turn ends — an attempt at 90% of its window is the
                        // one somebody wants to hear about while it is still
                        // running (gh#271). Held back once the turn is over:
                        // a straggling notification must not leave the run
                        // journal ending on something other than a `Done`.
                        if let Some(ctx) = context_event(&params)
                            && !done_current
                            && last_context != Some(ctx)
                        {
                            last_context = Some(ctx);
                            if !send(&event_tx, AgentEvent::ContextUsage(ctx)).await {
                                break 'main;
                            }
                        }
                    }

                    "turn/completed" => {
                        let id = turn_id(&params);
                        router.note_completed(&id);
                        if let Some(usage) = pending_usage.take()
                            && !send(&event_tx, usage).await
                        {
                            break 'main;
                        }
                        let error = turn_error_message(&params);
                        let status = if interrupted {
                            DoneStatus::Interrupted
                        } else if error.is_some() {
                            DoneStatus::Errored
                        } else {
                            DoneStatus::Completed
                        };
                        done_current = true;
                        if !send(
                            &event_tx,
                            AgentEvent::Done {
                                status,
                                result: None,
                                error,
                                session_id: Some(thread_id.clone()),
                            },
                        )
                        .await
                        {
                            break 'main;
                        }
                        if interrupted {
                            done_after_interrupt = true;
                            break 'main;
                        }
                        // Persistent session: a steer that lost the race with
                        // this turn's end becomes the next turn now; otherwise
                        // stay alive for the mailbox — the caller owns teardown.
                        if let Some(text) = queued_steers.pop_front() {
                            if !steer_as_new_turn(
                                &client,
                                turn_params(&text),
                                &mut router,
                                &event_tx,
                                &mut assistant_message_id,
                                &mut done_current,
                            )
                            .await
                            {
                                break 'main;
                            }
                        } else if !steering_open {
                            break 'main;
                        }
                    }

                    "turn/failed" => {
                        router.note_completed(&turn_id(&params));
                        if let Some(usage) = pending_usage.take()
                            && !send(&event_tx, usage).await
                        {
                            break 'main;
                        }
                        done_current = true;
                        if interrupted {
                            done_after_interrupt = true;
                        }
                        let _ = send(
                            &event_tx,
                            AgentEvent::Done {
                                status: if interrupted {
                                    DoneStatus::Interrupted
                                } else {
                                    DoneStatus::Errored
                                },
                                result: None,
                                error: Some(
                                    turn_error_message(&params)
                                        .unwrap_or_else(|| "Codex turn failed".into()),
                                ),
                                session_id: Some(thread_id.clone()),
                            },
                        )
                        .await;
                        break 'main;
                    }

                    "turn/aborted" => {
                        router.note_completed(&turn_id(&params));
                        done_current = true;
                        if interrupted {
                            done_after_interrupt = true;
                        }
                        let _ = send(
                            &event_tx,
                            AgentEvent::Done {
                                status: DoneStatus::Interrupted,
                                result: None,
                                error: None,
                                session_id: Some(thread_id.clone()),
                            },
                        )
                        .await;
                        break 'main;
                    }

                    "error" => {
                        let message = params
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("Codex error")
                            .to_owned();
                        if !send(&event_tx, AgentEvent::Error { message }).await {
                            break 'main;
                        }
                    }

                    // thread/status, mcpServer startup, account noise, … —
                    // unknown notification methods are tolerated by design.
                    _ => {}
                },

                Some(Incoming::Request { id, method, params }) => {
                    handle_server_request(
                        &client,
                        id,
                        &method,
                        &params,
                        request.auto_approve,
                        &request_input,
                    );
                }

                // stdout EOF or reader gone: the app server exited.
                Some(Incoming::Eof) | None => break 'main,
            },

            steer = steering.recv(), if steering_open && !interrupted => match steer {
                Some(msg) => {
                    let text = msg.prompt;
                    if let Some(expected) = router.active.clone() {
                        let steer_params = json!({
                            "threadId": thread_id,
                            "expectedTurnId": expected,
                            "input": [{ "type": "text", "text": text }],
                        });
                        match client.request("turn/steer", steer_params).await {
                            Ok(_) => {
                                let (prev, next) = rotate(&mut assistant_message_id);
                                if !send(
                                    &event_tx,
                                    AgentEvent::Steered {
                                        assistant_message_id: Some(prev),
                                        next_assistant_message_id: Some(next),
                                    },
                                )
                                .await
                                {
                                    break 'main;
                                }
                            }
                            // A failed `turn/steer` does NOT mean the text is
                            // bad: most commonly the active turn finished
                            // between the UI send and this request. Queue it
                            // for redelivery as the next `turn/start` when the
                            // expected turn's end arrives (also the safe
                            // fallback for older Codex without steering).
                            Err(e) => {
                                tracing::debug!(
                                    target: "comet_harness::codex",
                                    "turn/steer rejected (queued as next turn): {e}"
                                );
                                if router.active.as_deref() == Some(expected.as_str())
                                    && !router.is_completed(&expected)
                                {
                                    queued_steers.push_back(text);
                                } else if !steer_as_new_turn(
                                    &client,
                                    turn_params(&text),
                                    &mut router,
                                    &event_tx,
                                    &mut assistant_message_id,
                                    &mut done_current,
                                )
                                .await
                                {
                                    break 'main;
                                }
                            }
                        }
                    } else if !steer_as_new_turn(
                        &client,
                        turn_params(&text),
                        &mut router,
                        &event_tx,
                        &mut assistant_message_id,
                        &mut done_current,
                    )
                    .await
                    {
                        break 'main;
                    }
                }
                None => {
                    // Mailbox closed (the caller's graceful idle-reap): finish
                    // once nothing is in flight — mirrors codex.ts's steer loop
                    // `finish()` on a null take.
                    steering_open = false;
                    if router.active.is_none() && queued_steers.is_empty() {
                        break 'main;
                    }
                }
            },

            _ = interrupt.cancelled(), if !interrupt_sent => {
                interrupt_sent = true;
                interrupted = true;
                if let Some(turn) = router.active.clone() {
                    let client = client.clone();
                    let thread = thread_id.clone();
                    tokio::spawn(async move {
                        if let Err(e) = client
                            .request("turn/interrupt", json!({ "threadId": thread, "turnId": turn }))
                            .await
                        {
                            tracing::debug!(
                                target: "comet_harness::codex",
                                "turn/interrupt failed (escalation will reap): {e}"
                            );
                        }
                    });
                    // Escalate if the app server doesn't wind down (turn/aborted)
                    // within the grace periods: SIGTERM, then SIGKILL.
                    if let Some(pid) = child.id() {
                        escalation = Some(tokio::spawn(async move {
                            tokio::time::sleep(interrupt_grace).await;
                            send_signal(pid, Signal::Term);
                            tokio::time::sleep(kill_grace).await;
                            send_signal(pid, Signal::Kill);
                        }));
                    }
                } else {
                    // Idle between turns: nothing to interrupt — the terminal
                    // bookkeeping below still guarantees Done { Interrupted }.
                    break 'main;
                }
            },

            _ = event_tx.closed() => break 'main,
        }
    }

    // Terminal bookkeeping: never end the stream without a Done unless the
    // consumer already hung up.
    if !event_tx.is_closed() {
        if interrupted && !done_after_interrupt {
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: Some(thread_id.clone()),
                }))
                .await;
        } else if !interrupted && !done_current {
            // A child KILLED mid-turn (OS memory pressure, `killall codex`)
            // must not read as a silent success — codex.ts's signal-death
            // handling, reduced to the turn-in-flight case.
            let status = child.try_wait().ok().flatten();
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(crate::crash_message(
                        "codex app-server",
                        status,
                        &stderr_tail,
                    )),
                    session_id: Some(thread_id.clone()),
                }))
                .await;
        }
    }

    shutdown_child(&mut child, kill_grace).await;
    if let Some(handle) = escalation {
        handle.abort();
    }
}

/// Deliver a steer as a fresh `turn/start` on the same thread (the fallback
/// leg of the steer race, and the between-turns delivery path). Returns false
/// when the loop should end (turn/start failed or the consumer hung up).
async fn steer_as_new_turn(
    client: &RpcClient,
    params: Value,
    router: &mut TurnRouter,
    event_tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
    assistant_message_id: &mut String,
    done_current: &mut bool,
) -> bool {
    match start_turn(client, params).await {
        Ok(id) => {
            router.adopt_started(id);
            *done_current = false;
            let (prev, next) = rotate(assistant_message_id);
            send(
                event_tx,
                AgentEvent::Steered {
                    assistant_message_id: Some(prev),
                    next_assistant_message_id: Some(next),
                },
            )
            .await
        }
        Err(e) => {
            let _ = send(
                event_tx,
                AgentEvent::Error {
                    message: format!("Steering failed: {e}"),
                },
            )
            .await;
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Approvals (approval-as-input parity with comet's UX)
// ---------------------------------------------------------------------------

type RequestInputFn = Box<
    dyn Fn(Vec<UserInputQuestion>) -> tokio::sync::oneshot::Receiver<Vec<UserInputAnswer>>
        + Send
        + Sync,
>;

/// Serve one server→client request. Approval requests round-trip through
/// `request_input` as a synthesized yes/no question (in a subtask so the
/// message loop keeps flowing); with `auto_approve` they're accepted outright
/// (belt to the wire-level `approvalPolicy: "never"`). Anything else is
/// rejected as unsupported so the server never wedges awaiting a reply.
fn handle_server_request(
    client: &RpcClient,
    id: Value,
    method: &str,
    params: &Value,
    auto_approve: bool,
    request_input: &Arc<RequestInputFn>,
) {
    let is_approval = matches!(
        method,
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval"
    );
    if !is_approval {
        tracing::debug!(
            target: "comet_harness::codex",
            "unhandled server request: {method}"
        );
        client.respond_error(&id, -32601, &format!("unsupported method: {method}"));
        return;
    }
    if auto_approve {
        client.respond(&id, json!({ "decision": "accept" }));
        return;
    }

    let question = approval_question(method, params);
    let client = client.clone();
    let request_input = Arc::clone(request_input);
    tokio::spawn(async move {
        // The engine's input bridge owns the `InputRequested`/`InputResolved`
        // lifecycle (it mints the request id the resolver is parked under);
        // emitting our own copy here doubled the doc's input part with an id
        // `respond_input` could never match.
        //
        // A dropped sender (caller went away) degrades to a decline so the
        // agent is unblocked — never silently allowed.
        let answers = (request_input)(vec![question.clone()])
            .await
            .unwrap_or_default();
        let accept = answers.iter().any(|a| {
            a.question_id == question.id && a.labels.iter().any(|l| l.eq_ignore_ascii_case("yes"))
        });
        client.respond(
            &id,
            json!({ "decision": if accept { "accept" } else { "decline" } }),
        );
    });
}

/// Synthesize the yes/no question an approval request surfaces to the user.
fn approval_question(method: &str, params: &Value) -> UserInputQuestion {
    let (header, question) = if method.contains("commandExecution") {
        let command = match params.get("command") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(parts)) => parts
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
            _ => String::new(),
        };
        (
            "Approve command".to_owned(),
            if command.is_empty() {
                "Codex wants to run a command. Allow it?".to_owned()
            } else {
                format!("Codex wants to run `{command}`. Allow it?")
            },
        )
    } else {
        let paths: Vec<&str> = params
            .get("changes")
            .and_then(Value::as_array)
            .map(|a| a.as_slice())
            .unwrap_or_default()
            .iter()
            .filter_map(|c| c.get("path").and_then(Value::as_str))
            .collect();
        (
            "Approve file change".to_owned(),
            if paths.is_empty() {
                "Codex wants to modify files. Allow it?".to_owned()
            } else {
                format!("Codex wants to modify {}. Allow it?", paths.join(", "))
            },
        )
    };
    UserInputQuestion {
        id: new_message_id(),
        header,
        question,
        options: vec!["Yes".into(), "No".into()],
        multi_select: false,
    }
}

// ---------------------------------------------------------------------------
// Child lifecycle
// ---------------------------------------------------------------------------

/// Reap the child: graceful SIGTERM first, SIGKILL after `kill_grace`.
/// (`kill_on_drop` remains the last-resort backstop.)
async fn shutdown_child(child: &mut Child, kill_grace: Duration) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    if let Some(pid) = child.id() {
        send_signal(pid, Signal::Term);
        if tokio::time::timeout(kill_grace, child.wait()).await.is_ok() {
            return;
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[derive(Clone, Copy)]
enum Signal {
    Term,
    Kill,
}

#[cfg(unix)]
fn send_signal(pid: u32, signal: Signal) {
    let sig = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // SAFETY: plain kill(2) on a pid we spawned and have not yet reaped.
    unsafe {
        libc::kill(pid as libc::pid_t, sig);
    }
}

#[cfg(not(unix))]
fn send_signal(_pid: u32, _signal: Signal) {
    // No SIGTERM off unix; `start_kill`/`kill_on_drop` handle termination.
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn slashed_branch_worktrees_are_detected_for_sandbox_escalation() {
        let tmp = tempfile::tempdir().unwrap();
        let make = |name: &str, branch: &str| {
            let wt = tmp.path().join(name);
            let admin = tmp.path().join(format!("{name}-admin"));
            std::fs::create_dir_all(&wt).unwrap();
            std::fs::create_dir_all(&admin).unwrap();
            std::fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
            std::fs::write(admin.join("HEAD"), format!("ref: refs/heads/{branch}\n")).unwrap();
            wt.display().to_string()
        };
        assert!(worktree_on_slashed_branch(&make("slashed", "wing/prd-5645")));
        assert!(!worktree_on_slashed_branch(&make("plain", "brave-ember")));
        // A main checkout (`.git` DIRECTORY) never escalates.
        let main = tmp.path().join("main");
        std::fs::create_dir_all(main.join(".git")).unwrap();
        assert!(!worktree_on_slashed_branch(&main.display().to_string()));
        // Detached HEAD (raw sha) never escalates.
        let detached = make("detached", "x");
        std::fs::write(
            tmp.path().join("detached-admin").join("HEAD"),
            "0ba950848abc\n",
        )
        .unwrap();
        assert!(!worktree_on_slashed_branch(&detached));
        assert!(!worktree_on_slashed_branch(""));
        assert!(!worktree_on_slashed_branch("/nonexistent/path"));
    }

    /// The measurement behind [`git_writable_roots`], as a fixture: codex's
    /// `workspace-write` denies exactly `<cwd>/.git`, so a main checkout needs
    /// that path named or the agent cannot commit — and a worktree, whose git
    /// dir is somewhere else entirely, is the case that already worked.
    #[test]
    fn a_main_checkouts_git_dir_is_named_writable_and_a_worktrees_pair_is_too() {
        let tmp = tempfile::tempdir().unwrap();

        // A main checkout: `.git` is a directory, and it IS the deny target.
        let main = tmp.path().join("main");
        std::fs::create_dir_all(main.join(".git")).unwrap();
        let roots = git_writable_roots(&main.display().to_string());
        assert_eq!(roots, vec![main.join(".git")]);

        // A linked worktree: `.git` is a pointer file, the admin dir lives
        // under the main checkout, and `commondir` points at the shared one.
        // Both are returned — the admin dir is where a commit writes and the
        // common dir is where a fetch does, and naming one would fix `commit`
        // while leaving `pull` broken.
        let admin = main.join(".git").join("worktrees").join("wt");
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::write(admin.join("commondir"), "../..\n").unwrap();
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
        let roots = git_writable_roots(&wt.display().to_string());
        assert_eq!(roots.len(), 2, "git dir and common dir: {roots:?}");
        assert_eq!(roots[0], admin);
        // Normalized, not `…/worktrees/wt/../..` — the sandbox matches paths
        // textually, so an unresolved one would grant nothing.
        assert_eq!(
            roots[1],
            std::fs::canonicalize(main.join(".git")).unwrap(),
            "the common dir is resolved, not left as a `..` walk"
        );

        // Nothing to name where there is no checkout, and nothing to crash on.
        assert!(git_writable_roots(&tmp.path().join("bare").display().to_string()).is_empty());
        assert!(git_writable_roots("").is_empty());
        assert!(git_writable_roots("/nonexistent/path").is_empty());
    }

    /// The writable roots ride the wire policy, and only where the level has a
    /// field for them.
    #[test]
    fn writable_roots_ride_workspace_write_and_no_other_level() {
        let roots = vec![PathBuf::from("/repo/.git")];
        let policy = sandbox_policy_value(comet_proto::SandboxLevel::WorkspaceWrite, &roots);
        assert_eq!(policy["type"], "workspaceWrite");
        assert_eq!(policy["writableRoots"], json!(["/repo/.git"]));
        // Full access needs no list, and read-only would not honour one.
        let policy = sandbox_policy_value(comet_proto::SandboxLevel::DangerFullAccess, &roots);
        assert!(policy.get("writableRoots").is_none());
        let policy = sandbox_policy_value(comet_proto::SandboxLevel::ReadOnly, &roots);
        assert!(policy.get("writableRoots").is_none());
        // An empty list is omitted rather than sent as `[]` — nothing to say.
        let policy = sandbox_policy_value(comet_proto::SandboxLevel::WorkspaceWrite, &[]);
        assert!(policy.get("writableRoots").is_none());
    }

    #[test]
    fn the_cli_version_is_read_off_the_line_codex_prints() {
        assert_eq!(
            parse_codex_version("codex-cli 0.147.0\n"),
            Some((0, 147, 0))
        );
        assert_eq!(parse_codex_version("codex-cli 0.144.3"), Some((0, 144, 3)));
        assert_eq!(parse_codex_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(
            parse_codex_version("codex-cli 0.148.0-rc.1"),
            Some((0, 148, 0))
        );
        // Anything this cannot read three numbers out of is `None`, never a
        // half-parsed version — the only thing it gates is a sandbox drop.
        assert_eq!(parse_codex_version("codex-cli 0.147"), None);
        assert_eq!(parse_codex_version("some other tool"), None);
        assert_eq!(parse_codex_version(""), None);
    }

    /// The escalation's whole reason to exist is a bug fixed in 0.147.0, and
    /// the board's default branch template puts every dispatch into its blast
    /// radius. The gate is what keeps it off a current box.
    #[test]
    fn the_worktree_escalation_is_gated_on_a_codex_old_enough_to_need_it() {
        let needs = |v: CliVersion| v < WORKTREE_MOUNT_FIXED_IN;
        assert!(needs((0, 144, 3)), "the version it was verified broken on");
        assert!(!needs((0, 147, 0)), "the version it was verified fixed on");
        assert!(!needs((0, 148, 0)));
        assert!(!needs((1, 0, 0)));
    }

    #[test]
    fn approval_questions_are_yes_no() {
        let q = approval_question(
            "item/commandExecution/requestApproval",
            &json!({"itemId": "c1", "command": "rm -rf /tmp/x"}),
        );
        assert_eq!(q.header, "Approve command");
        assert!(q.question.contains("rm -rf /tmp/x"));
        assert_eq!(q.options, vec!["Yes".to_string(), "No".to_string()]);
        assert!(!q.multi_select);

        let q = approval_question(
            "item/fileChange/requestApproval",
            &json!({"changes": [{"path": "/a.rs"}, {"path": "/b.rs"}]}),
        );
        assert_eq!(q.header, "Approve file change");
        assert!(q.question.contains("/a.rs, /b.rs"));

        // Command as argv array joins with spaces.
        let q = approval_question(
            "item/commandExecution/requestApproval",
            &json!({"command": ["git", "push", "--force"]}),
        );
        assert!(q.question.contains("git push --force"));
    }

    #[test]
    fn turn_router_never_revives_completed_turns() {
        let mut r = TurnRouter::default();
        r.note_completed("t-1");
        // The turn/start response arriving after turn/completed must not
        // resurrect the turn.
        r.adopt_started("t-1".into());
        assert_eq!(r.active, None);
        // Nor may a late turn/started notification.
        r.note_started("t-1".into());
        assert_eq!(r.active, None);

        r.note_started("t-2".into());
        assert_eq!(r.active.as_deref(), Some("t-2"));
        // A replacement started turn retires the stale one.
        r.note_started("t-3".into());
        assert_eq!(r.active.as_deref(), Some("t-3"));
        assert!(r.is_completed("t-2"));
    }
}
