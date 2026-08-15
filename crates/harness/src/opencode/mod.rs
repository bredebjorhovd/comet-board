//! Opencode harness: spawns the installed `opencode` CLI as `opencode serve`
//! (a headless HTTP+SSE server) and drives it over its own REST/SSE protocol —
//! the same interface the opencode TUI/web/IDE clients talk to.
//!
//! Integration notes (verified against opencode 1.18.x, researched from the
//! `sst/opencode` source + a live server):
//!
//! - `opencode serve --port 0` prints `opencode server listening on
//!   http://127.0.0.1:<port>` on stdout; the server has no stdio channel, so
//!   unlike Codex's JSON-RPC app-server there is no handshake — REST + SSE.
//! - A run = `POST /session` (or resume) → `POST /session/{id}/prompt_async`
//!   → read the instance `/event` SSE stream. Text streams as
//!   `message.part.delta` (`properties.field` = "text"|"reasoning"); tool
//!   parts walk `message.part.updated` states pending → running → completed;
//!   a turn ends on `session.status`/`session.idle`; `session.error` (name
//!   `MessageAbortedError`) is an abort.
//! - Steering: a `prompt_async` into a BUSY session is delivered to the live
//!   agent as a steer (the IDE-style injection; `session.next.prompted`
//!   `delivery: "steer"`); into an idle one it starts the next turn. The
//!   session is persistent across turns while the steering mailbox lives.
//! - Interrupt: `POST /session/{id}/abort` → `session.error` (aborted) +
//!   idle; escalating to SIGTERM → SIGKILL if the server doesn't wind down,
//!   and the stream always ends with `Done { status: Interrupted }`.
//! - Permissions: opencode asks before tools; comet sessions run unattended
//!   (the sandbox is the guardrail, matching the Claude/Codex adapters), so
//!   every `permission.asked` is auto-allowed.
//! - `COMET_BOARD_CHAT_ID` rides the child env (the board-provenance
//!   convention; see [`crate::RunControls::chat_id`]).
//! - Context fullness ([`comet_proto::ContextUsage`], gh#271) is **not
//!   reported here**, deliberately. The SSE stream volunteers no window: an
//!   assistant `message.updated` carries the message's `tokens`, and the
//!   window they fill lives in the provider catalog (`/config/providers` →
//!   `model.limit.context`), which this adapter does not read. A fullness
//!   with a denominator we inferred rather than were told is exactly the
//!   number this signal exists to not be, so the harness stays silent and the
//!   board renders the absence — see `ContextUsage::max_tokens`.

mod catalog;
mod client;
mod normalize;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::Value;
use tokio::io::AsyncBufReadExt;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use comet_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode,
    UserInputAnswer, UserInputQuestion,
};

use crate::{Harness, HarnessError, RunControls};
use catalog::{REASONING_LEVELS, static_models, to_effort};
use client::{Client, ModelRef};
use normalize::Event;

/// Locate the device's installed opencode CLI: `OPENCODE_EXECUTABLE`, then
/// PATH, then common install locations GUI launches miss.
pub(crate) fn resolve_opencode_executable() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("OPENCODE_EXECUTABLE")
        && !p.is_empty()
    {
        return Some(PathBuf::from(p));
    }
    let exe = if cfg!(windows) { "opencode.exe" } else { "opencode" };
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.join(exe))
                .collect()
        })
        .unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        candidates.push(home.join(".local").join("bin").join(exe));
        candidates.push(home.join(".npm-global").join("bin").join(exe));
    }
    candidates.push(PathBuf::from("/opt/homebrew/bin/opencode"));
    candidates.push(PathBuf::from("/usr/local/bin/opencode"));
    candidates.extend(
        crate::node_version_manager_bins()
            .into_iter()
            .map(|d| d.join(exe)),
    );
    candidates.into_iter().find(|p| p.exists())
}

/// Parse `opencode server listening on http://127.0.0.1:<port>`.
fn parse_listening_port(line: &str) -> Option<u16> {
    let marker = "listening on http://127.0.0.1:";
    let start = line.find(marker)? + marker.len();
    let rest = &line[start..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// The opencode harness. Construct with [`OpencodeHarness::new`]; tests point
/// it at a fake CLI with [`OpencodeHarness::with_executable`].
pub struct OpencodeHarness {
    executable: Option<PathBuf>,
    /// Grace between `abort` and SIGTERM.
    interrupt_grace: Duration,
    /// Grace between SIGTERM and SIGKILL.
    kill_grace: Duration,
    /// Total deadline for the short JSON requests; the `/event` SSE stream is
    /// exempt (a total timeout would close a busy mid-run feed, gh#46).
    request_timeout: Duration,
    /// Short-TTL cache for the live model catalog (models() spawns a server).
    models_cache: Mutex<Option<(Instant, Vec<Model>)>>,
}

impl Default for OpencodeHarness {
    fn default() -> Self {
        Self {
            executable: None,
            interrupt_grace: Duration::from_secs(2),
            kill_grace: Duration::from_secs(3),
            request_timeout: client::REQUEST_TIMEOUT,
            models_cache: Mutex::new(None),
        }
    }
}

impl OpencodeHarness {
    pub fn new() -> Self {
        Self::default()
    }

    /// Use a fixed CLI binary instead of PATH/known-location resolution.
    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        self
    }

    /// Tune the abort→SIGTERM→SIGKILL escalation timing.
    pub fn with_graces(mut self, interrupt_grace: Duration, kill_grace: Duration) -> Self {
        self.interrupt_grace = interrupt_grace;
        self.kill_grace = kill_grace;
        self
    }

    /// Override the total deadline for the short JSON requests (default 20s).
    /// The `/event` SSE stream ignores this — it only carries a read timeout —
    /// which is what lets tests shrink the deadline below a turn's length and
    /// assert the stream survives it (gh#46).
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    fn resolve_executable(&self) -> Result<PathBuf, HarnessError> {
        if let Some(p) = &self.executable {
            return Ok(p.clone());
        }
        resolve_opencode_executable().ok_or_else(|| {
            HarnessError::NotInstalled(
                "opencode (searched PATH, ~/.local/bin, ~/.npm-global/bin, \
                 /opt/homebrew/bin, /usr/local/bin, and fnm/nvm/volta/pnpm/bun install \
                 dirs; set OPENCODE_EXECUTABLE to override)"
                    .into(),
            )
        })
    }
}

#[async_trait]
impl Harness for OpencodeHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Opencode
    }
    fn display_name(&self) -> &str {
        // Must match the engine registry's lazy descriptor so the catalog
        // entry doesn't change after the first resolve.
        "OpenCode"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    /// `prompt_async` into a busy session steers the live turn (the IDE-style
    /// injection); an idle session starts the next turn on the same session.
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        REASONING_LEVELS
    }

    /// The live catalog served by `opencode serve` (`GET /config/providers`),
    /// via a short-lived server; falls back to the static snapshot when the
    /// endpoint isn't reachable. Requires an installed CLI so an absent binary
    /// surfaces as [`HarnessError::NotInstalled`].
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        if let Some((at, models)) = self
            .models_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            && at.elapsed() < Duration::from_secs(60)
        {
            return Ok(models.clone());
        }
        let exe = self.resolve_executable()?;
        let (mut child, base, _stderr) = spawn_server(&exe, None, None, None, &[], &[]).await?;
        let models = {
            let client = Client::new(base);
            client
                .models()
                .await
                .unwrap_or_else(static_models)
        };
        shutdown_child(&mut child, self.kill_grace).await;
        let cached = models.clone();
        *self
            .models_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((Instant::now(), cached));
        Ok(models)
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let exe = self.resolve_executable()?;
        let cwd = if request.cwd.is_empty() {
            None
        } else {
            Some(request.cwd.as_str())
        };
        let (mut child, base, stderr_tail) = spawn_server(
            &exe,
            cwd,
            controls.chat_id.as_deref(),
            controls.push.as_ref(),
            &controls.bin_dirs,
            &controls.mcp_servers,
        )
        .await?;
        let client = Client::with_timeouts(base, self.request_timeout, client::STREAM_READ_TIMEOUT);

        let session_id = if let Some(resume) = &request.resume {
            // opencode session ids (`ses_…`) are durable in its db; resume the
            // same conversation (a foreign id fails the prompt below and the
            // engine's failed-resume retry starts fresh).
            resume.clone()
        } else {
            match client.create_session().await {
                Ok(id) => id,
                Err(e) => {
                    shutdown_child(&mut child, self.kill_grace).await;
                    return Err(e);
                }
            }
        };

        // Connect the event stream BEFORE the prompt so no run events are lost.
        let event_res = match client.event_stream().await {
            Ok(res) => res,
            Err(e) => {
                shutdown_child(&mut child, self.kill_grace).await;
                return Err(e);
            }
        };

        // The terms this run is on, ahead of anything opencode says (§gh#349).
        // Nothing below passes `request.sandbox` to the server — opencode is
        // spawned as an ordinary child process with the run device's own
        // permissions, and every tool it asks about is approved (see the
        // approval handler in `run_session`). The requested level is a
        // preference this adapter has never been able to honour, and the board
        // should be told that rather than shown the request back.
        let sandbox = comet_proto::SandboxReport::widened(
            request.sandbox,
            comet_proto::SandboxLevel::DangerFullAccess,
            "opencode is run as an ordinary child process with no sandbox, and this adapter \
             approves every tool it is asked about",
        );
        let (event_tx, event_rx) = mpsc::channel::<Result<AgentEvent, HarnessError>>(256);
        let model_ref = ModelRef {
            model: split_model(&request.model),
            variant: to_effort(request.reasoning).map(str::to_owned),
        };
        tokio::spawn(run_session(Session {
            child,
            client,
            event_tx,
            controls,
            request,
            session_id,
            interrupt_grace: self.interrupt_grace,
            kill_grace: self.kill_grace,
            stderr_tail,
            model_ref,
            event_res,
        }));

        Ok(
            futures::stream::once(async move { Ok(AgentEvent::Sandbox(sandbox)) })
                .chain(futures::stream::unfold(event_rx, |mut rx| async move {
                    rx.recv().await.map(|ev| (ev, rx))
                }))
                .boxed(),
        )
    }
}

/// Split a `provider/model` catalog id into the wire's `{providerID, modelID}`;
/// a bare id assumes the `opencode` provider, and `None` means "use the
/// default model".
fn split_model(model: &Option<String>) -> Option<(String, String)> {
    model.as_ref().map(|m| match m.split_once('/') {
        Some((provider, model)) => (provider.to_string(), model.to_string()),
        None => ("opencode".to_string(), m.clone()),
    })
}

// ---------------------------------------------------------------------------
// Server lifecycle
// ---------------------------------------------------------------------------

/// Spawn `opencode serve --port 0` (ephemeral port, printed on stdout) and
/// wait for it to report the port and pass a health check. The port-reader
/// task keeps draining stdout forever so the child never blocks on a full pipe.
async fn spawn_server(
    exe: &std::path::Path,
    cwd: Option<&str>,
    chat_id: Option<&str>,
    push: Option<&crate::PushCredentials>,
    bin_dirs: &[std::path::PathBuf],
    mcp_servers: &[comet_proto::McpServer],
) -> Result<(Child, String, crate::StderrTail), HarnessError> {
    let mut cmd = Command::new(exe);
    cmd.arg("serve").arg("--port").arg("0").arg("--hostname").arg("127.0.0.1");
    crate::prepend_exe_dir_to_path(&mut cmd, exe);
    if let Some(chat_id) = chat_id {
        cmd.env("COMET_BOARD_CHAT_ID", chat_id);
    }
    if let Some(config) = opencode_inline_config(
        std::env::var("OPENCODE_CONFIG_CONTENT").ok().as_deref(),
        mcp_servers,
    )? {
        cmd.env("OPENCODE_CONFIG_CONTENT", config);
    }
    // The server is what runs the agent's tools, so the credentials — and the
    // directories the tools themselves live in — belong on it; opencode's own
    // process never pushes anything and never types `comet-board`.
    crate::prepend_dirs_to_path(&mut cmd, bin_dirs);
    if let Some(push) = push {
        push.apply(&mut cmd);
    }
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    cmd.stdin(Stdio::null())
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

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| HarnessError::Protocol("opencode serve has no stdout".into()))?;
    let stderr_tail = crate::StderrTail::default();
    if let Some(stderr) = child.stderr.take() {
        let tail = stderr_tail.clone();
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "comet_harness::opencode", "stderr: {line}");
                tail.push(&line);
            }
        });
    }

    let (port_tx, port_rx) = tokio::sync::oneshot::channel();
    let mut port_tx = Some(port_tx);
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!(target: "comet_harness::opencode", "stdout: {line}");
            if let Some(port) = parse_listening_port(&line)
                && let Some(tx) = port_tx.take()
            {
                let _ = tx.send(port);
            }
        }
    });
    let port = tokio::time::timeout(Duration::from_secs(30), port_rx)
        .await
        .map_err(|_| HarnessError::Protocol("timed out waiting for opencode serve port".into()))?
        .map_err(|_| {
            HarnessError::Protocol("opencode serve exited before reporting its port".into())
        })?;
    let base = format!("http://127.0.0.1:{port}");

    // The "listening" line prints before the HTTP server accepts; poll health.
    let client = Client::new(base.clone());
    for _ in 0..50 {
        if client.health().await {
            return Ok((child, base, stderr_tail));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let status = child.try_wait().ok().flatten();
    let message = crate::crash_message("opencode serve", status, &stderr_tail);
    shutdown_child(&mut child, Duration::from_secs(2)).await;
    Err(HarnessError::Protocol(format!(
        "opencode serve never became ready: {message}"
    )))
}

/// Merge route-specific MCP servers into opencode's highest-precedence inline
/// config (gh#273). Existing inline settings — including values supplied by an
/// operator through the environment — survive value-for-value; only same-named
/// MCP entries are replaced. Nothing is written to the checkout or user config.
fn opencode_inline_config(
    existing: Option<&str>,
    servers: &[comet_proto::McpServer],
) -> Result<Option<String>, HarnessError> {
    if servers.is_empty() {
        return Ok(None);
    }
    let mut config = match existing {
        Some(raw) => serde_json::from_str::<serde_json::Value>(raw).map_err(|e| {
            HarnessError::Protocol(format!("OPENCODE_CONFIG_CONTENT is not valid JSON: {e}"))
        })?,
        None => serde_json::json!({}),
    };
    let root = config.as_object_mut().ok_or_else(|| {
        HarnessError::Protocol("OPENCODE_CONFIG_CONTENT must be a JSON object".into())
    })?;
    let mcp = root
        .entry("mcp")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| {
            HarnessError::Protocol("OPENCODE_CONFIG_CONTENT.mcp must be an object".into())
        })?;
    for server in servers {
        let mut command = Vec::with_capacity(server.args.len() + 1);
        command.push(server.command.clone());
        command.extend(server.args.iter().cloned());
        mcp.insert(
            server.name.clone(),
            serde_json::json!({
                "type": "local",
                "command": command,
                "enabled": true,
            }),
        );
    }
    Ok(Some(config.to_string()))
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

struct Session {
    child: Child,
    client: Client,
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    controls: RunControls,
    request: RunRequest,
    session_id: String,
    interrupt_grace: Duration,
    kill_grace: Duration,
    stderr_tail: crate::StderrTail,
    model_ref: ModelRef,
    /// The `/event` SSE response; owned by the reader task.
    event_res: reqwest::Response,
}

fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// The turn went idle (or its feed ended) without the assistant message ever
/// settling — the server claimed a completion it never delivered (gh#37).
const STALLED_TURN: &str = "opencode went idle without completing the assistant message";

/// How long a run that is already erroring waits for a serve exit it may be
/// racing before settling on the [`STALLED_TURN`] wording (gh#79). A dying
/// serve closes its sockets and terminates near-simultaneously, so the idle /
/// EOF its death produces can reach us a hair before the exit is visible to
/// `try_wait` — more often on a fast Linux host than on macOS. This window is
/// far wider than that gap, and only a run headed for Errored ever pays it.
const EXIT_PROBE_GRACE: Duration = Duration::from_millis(500);

/// Probe the serve child for a crash, waiting up to `grace` for an exit we may
/// be racing (see [`EXIT_PROBE_GRACE`]; `Duration::ZERO` just asks). `Some(msg)`
/// — the child is gone, and this crash diagnosis (with its real exit status) is
/// what a human needs to read; `None` — still running, so the caller's own
/// wording stands.
async fn crash_if_exited(
    child: &mut Child,
    stderr_tail: &crate::StderrTail,
    grace: Duration,
) -> Option<String> {
    let status = match child.try_wait() {
        Ok(Some(status)) => Some(status),
        // ECHILD and friends: gone, status unknowable.
        Err(_) => None,
        // Still running as far as a bare probe can tell.
        Ok(None) if grace.is_zero() => return None,
        Ok(None) => match tokio::time::timeout(grace, child.wait()).await {
            Ok(Ok(status)) => Some(status),
            Ok(Err(_)) => None,
            // Outlived the grace: alive, not a crash.
            Err(_) => return None,
        },
    };
    Some(crate::crash_message("opencode serve", status, stderr_tail))
}

/// Rotate the assistant message id; returns (previous, next).
fn rotate(id: &mut String) -> (String, String) {
    let prev = std::mem::replace(id, new_message_id());
    (prev, id.clone())
}

async fn send(tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>, ev: AgentEvent) -> bool {
    tx.send(Ok(ev)).await.is_ok()
}

/// `SessionStarted` — emitted up front (and again after a fresh-session
/// fallback) so the engine's stored resume id tracks the real opencode session.
async fn announce_session(
    event_tx: &mpsc::Sender<Result<AgentEvent, HarnessError>>,
    request: &RunRequest,
    assistant_message_id: &str,
    session_id: &str,
) -> bool {
    send(
        event_tx,
        AgentEvent::SessionStarted {
            harness: HarnessId::Opencode,
            model: request.model.clone().unwrap_or_default(),
            tools: Vec::new(),
            cwd: request.cwd.clone(),
            session_id: session_id.to_string(),
            assistant_message_id: assistant_message_id.to_string(),
        },
    )
    .await
}

type RequestInputFn = Box<
    dyn Fn(Vec<UserInputQuestion>) -> tokio::sync::oneshot::Receiver<Vec<UserInputAnswer>>
        + Send
        + Sync,
>;

/// Read the `/event` SSE stream and pump parsed [`Event`]s (filtered to our
/// session) into the loop's channel.
async fn event_reader(
    res: reqwest::Response,
    tx: mpsc::Sender<Event>,
    session_id: &str,
) {
    let mut stream = Box::pin(res.bytes_stream());
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(target: "comet_harness::opencode", "event stream read failed: {e}");
                break;
            }
        };
        buf.extend_from_slice(&chunk);
        loop {
            let Some(idx) = buf.iter().position(|&b| b == b'\n') else {
                break;
            };
            let line: Vec<u8> = buf.drain(..=idx).collect();
            let line = String::from_utf8_lossy(&line[..line.len().saturating_sub(1)]);
            let line = line.trim();
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(data.trim()) else {
                continue;
            };
            let Some(type_) = value.get("type").and_then(Value::as_str) else {
                continue;
            };
            let props = value.get("properties").cloned().unwrap_or(Value::Null);
            // Events naming a session belong to ours alone (the instance
            // stream carries every session the server hosts).
            //
            // KNOWN BLIND SPOT (gh#280 fixed claude's half): opencode runs a
            // `task` subagent in a CHILD session with its own id, so its
            // events are dropped here — the same silence claude had. It is not
            // fixed the same way because this stream is shared: an unknown
            // sessionID may be our subagent OR another chat on the same
            // server, and beating liveness for someone else's run would be
            // worse than beating none. Attributing children needs the
            // `parentID` a `session.updated` carries, verified against a live
            // server.
            if let Some(sid) = props.get("sessionID").and_then(Value::as_str)
                && sid != session_id
            {
                continue;
            }
            if let Some(ev) = normalize::parse_event(type_, &props) {
                if tx.send(ev).await.is_err() {
                    return;
                }
            }
        }
    }
    let _ = tx.send(Event::Eof).await;
}

/// Answer a `question.asked` through the engine's input bridge, then reply on
/// the wire: one array of selected labels per question, in order.
async fn answer_questions(
    client: &Client,
    request_id: &str,
    questions: &[UserInputQuestion],
    request_input: &Arc<RequestInputFn>,
) {
    let answers = (request_input)(questions.to_vec()).await.unwrap_or_default();
    let mut by_index: HashMap<usize, Vec<String>> = HashMap::new();
    for answer in &answers {
        if let Some((_, idx)) = answer.question_id.rsplit_once(':')
            && let Ok(i) = idx.parse::<usize>()
        {
            by_index.insert(i, answer.labels.clone());
        }
    }
    let ordered: Vec<Vec<String>> = (0..questions.len())
        .map(|i| by_index.remove(&i).unwrap_or_default())
        .collect();
    if let Err(e) = client.reply_question(request_id, ordered).await {
        tracing::warn!(target: "comet_harness::opencode", "question reply failed: {e}");
    }
}

/// The per-run event loop: one task multiplexing the SSE stream, the steering
/// mailbox, the interrupt token, and consumer liveness.
async fn run_session(session: Session) {
    let Session {
        mut child,
        client,
        event_tx,
        controls,
        request,
        session_id,
        interrupt_grace,
        kill_grace,
        stderr_tail,
        model_ref,
        event_res,
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
        mcp_servers: _,
    } = controls;
    let request_input = Arc::new(request_input);

    // The reader task owns the SSE response; the loop drains its channel.
    let (incoming_tx, mut incoming) = mpsc::channel::<Event>(256);
    {
        let tx = incoming_tx.clone();
        let sid = session_id.clone();
        tokio::spawn(async move {
            event_reader(event_res, tx, &sid).await;
        });
    }

    let mut assistant_message_id = new_message_id();
    let mut session_id = session_id;

    // Announce the session up front so the engine stores the resume id before
    // any content streams (a fallback below re-announces a fresh session).
    if !announce_session(&event_tx, &request, &assistant_message_id, &session_id).await {
        shutdown_child(&mut child, kill_grace).await;
        return;
    }

    // The prompt fails on a STALE/foreign resume id (the session's gone from
    // opencode's db) — retry once as a fresh session, exactly like the engine's
    // failed-resume fallback for the other harnesses.
    if let Err(e) = client.prompt_async(&session_id, &request.prompt, &model_ref).await {
        if request.resume.is_some() {
            tracing::warn!(target: "comet_harness::opencode", resume = %request.resume.as_deref().unwrap_or(""), "resumed session rejected (starting fresh): {e}");
            match client.create_session().await {
                Ok(fresh) => {
                    session_id = fresh;
                    if !announce_session(&event_tx, &request, &assistant_message_id, &session_id)
                        .await
                    {
                        shutdown_child(&mut child, kill_grace).await;
                        return;
                    }
                }
                Err(create_err) => {
                    let _ = send(
                        &event_tx,
                        AgentEvent::Error {
                            message: format!(
                                "opencode failed to start the run: {create_err}"
                            ),
                        },
                    )
                    .await;
                    let _ = send(
                        &event_tx,
                        AgentEvent::Done {
                            status: DoneStatus::Errored,
                            result: None,
                            error: Some(create_err.to_string()),
                            session_id: Some(session_id.clone()),
                        },
                    )
                    .await;
                    shutdown_child(&mut child, kill_grace).await;
                    return;
                }
            }
        }
        if let Err(e2) = client.prompt_async(&session_id, &request.prompt, &model_ref).await {
            let _ = send(
                &event_tx,
                AgentEvent::Error {
                    message: format!("opencode failed to start the run: {e2}"),
                },
            )
            .await;
            let _ = send(
                &event_tx,
                AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(e2.to_string()),
                    session_id: Some(session_id.clone()),
                },
            )
            .await;
            shutdown_child(&mut child, kill_grace).await;
            return;
        }
    }

    // ---- main loop --------------------------------------------------------
    // ToolCall is emitted once per part (first non-empty input); ToolResult
    // when the part settles. Assistant-message boundaries dedupe (opencode
    // re-emits `message.updated` with `time.completed`).
    let mut tool_calls: HashSet<String> = HashSet::new();
    let mut completed_msgs: HashSet<String> = HashSet::new();
    // A turn is in flight from the initial prompt until the next idle.
    let mut turn_active = true;
    let mut interrupted = false;
    let mut interrupt_sent = false;
    // Done emitted for the current turn (idle fires per busy→idle cycle).
    let mut done_for_turn = false;
    let mut done_after_interrupt = false;
    // A non-aborted `session.error` failed the current turn.
    let mut turn_error: Option<String> = None;
    // The in-flight assistant message (the last one `message.updated` named)
    // and whether it reached completion (`time.completed`) — the signal that a
    // turn actually settled. An `Idle` that lands before the current message
    // completed is a mid-stream stall (gh#37), not a completion.
    let mut current_msg: Option<String> = None;
    let mut current_msg_completed = false;
    let mut steering_open = true;
    let mut escalation: Option<tokio::task::JoinHandle<()>> = None;

    'main: loop {
        tokio::select! {
            ev = incoming.recv() => match ev {
                Some(Event::Delta { field, delta }) => {
                    let event = if field == "reasoning" {
                        AgentEvent::ReasoningDelta { text: delta }
                    } else {
                        AgentEvent::TextDelta { text: delta }
                    };
                    if !send(&event_tx, event).await { break 'main; }
                }

                Some(Event::Tool { part_id, status, input, exit, title }) => {
                    let empty_input = input
                        .as_ref()
                        .is_some_and(|v| v.as_object().is_some_and(|m| m.is_empty()));
                    if !empty_input
                        && (status == "pending" || status == "running")
                        && !tool_calls.contains(&part_id)
                        && let Some(input) = &input
                    {
                        tool_calls.insert(part_id.clone());
                        let call = normalize::tool_call_from_input(&part_id, input, &title);
                        if !send(&event_tx, AgentEvent::ToolCall { id: part_id.clone(), call }).await {
                            break 'main;
                        }
                    }
                    if matches!(status.as_str(), "completed" | "error" | "failed") {
                        let is_error = status != "completed" || exit.is_some_and(|e| e != 0);
                        if !send(&event_tx, AgentEvent::ToolResult { id: part_id, is_error }).await {
                            break 'main;
                        }
                    }
                }

                Some(Event::MessageUpdated { info }) => {
                    let id = normalize::assistant_message_id(&info);
                    let completed = normalize::assistant_message_completed(&info);
                    // Track the in-flight assistant message: a new id starts a
                    // fresh (not yet settled) message; a completion settles it.
                    // The completion must precede the turn's idle for the turn
                    // to count as Completed (gh#37).
                    if current_msg.as_deref() != Some(id.as_str()) {
                        current_msg = Some(id.clone());
                        current_msg_completed = completed;
                    } else if completed {
                        current_msg_completed = true;
                    }
                    if completed && !completed_msgs.contains(&id) {
                        completed_msgs.insert(id.clone());
                        if !send(&event_tx, AgentEvent::AssistantMessageCompleted { assistant_message_id: id }).await {
                            break 'main;
                        }
                    }
                }

                Some(Event::Busy) => {
                    turn_active = true;
                    done_for_turn = false;
                }

                Some(Event::Idle) => {
                    let turn_was_active = turn_active;
                    turn_active = false;
                    if interrupted {
                        if !done_after_interrupt {
                            done_after_interrupt = true;
                            let _ = send(
                                &event_tx,
                                AgentEvent::Done {
                                    status: DoneStatus::Interrupted,
                                    result: None,
                                    error: None,
                                    session_id: Some(session_id.clone()),
                                },
                            )
                            .await;
                        }
                        break 'main;
                    }
                    if !done_for_turn {
                        done_for_turn = true;
                        let mut error = turn_error.take();
                        // A turn that was in flight but whose assistant message
                        // never reached completion (`message.updated` with
                        // `time.completed`) before the idle is a stall, not a
                        // finished turn — report it Errored rather than a
                        // spurious Completed (gh#37). Unless the serve is in
                        // fact dead: then the crash, with its exit status, is
                        // the honest diagnosis and "went idle" would send the
                        // reader after the wrong problem (gh#79).
                        if error.is_none() && turn_was_active && !current_msg_completed {
                            error = Some(
                                match crash_if_exited(&mut child, &stderr_tail, EXIT_PROBE_GRACE)
                                    .await
                                {
                                    Some(crash) => crash,
                                    None => STALLED_TURN.into(),
                                },
                            );
                        }
                        let status = if error.is_some() {
                            DoneStatus::Errored
                        } else {
                            DoneStatus::Completed
                        };
                        if !send(
                            &event_tx,
                            AgentEvent::Done {
                                status,
                                result: None,
                                error,
                                session_id: Some(session_id.clone()),
                            },
                        )
                        .await
                        {
                            break 'main;
                        }
                    }
                    // Persistent session: stay parked for the steering mailbox
                    // unless the caller already closed it (idle-reap).
                    if !steering_open {
                        break 'main;
                    }
                }

                Some(Event::Error { message, aborted }) => {
                    if aborted {
                        interrupted = true;
                    } else {
                        turn_error = Some(message.clone());
                        if !send(&event_tx, AgentEvent::Error { message }).await {
                            break 'main;
                        }
                    }
                }

                Some(Event::Question { request_id, questions }) => {
                    answer_questions(&client, &request_id, &questions, &request_input).await;
                }

                Some(Event::Permission { request_id, .. }) => {
                    // Parity with codex's wire-level `approvalPolicy: "never"`
                    // (and Claude's auto-approved can_use_tool): comet runs
                    // unattended — the sandbox is the guardrail, so every
                    // permission is auto-allowed.
                    let _ = client.reply_permission(&request_id, "always").await;
                }

                // SSE EOF or reader gone: the server exited, or simply closed
                // its feed (idle-parked / one-shot subscription). The terminal
                // bookkeeping below probes try_wait to tell the two apart.
                Some(Event::Eof) | None => break 'main,
            },

            steer = steering.recv(), if steering_open && !interrupted => match steer {
                Some(msg) => {
                    let text = msg.prompt;
                    // A prompt_async into the session steers a live turn or
                    // starts the next one when idle (the IDE-style delivery).
                    match client.prompt_async(&session_id, &text, &model_ref).await {
                        Ok(()) => {
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
                            turn_active = true;
                            done_for_turn = false;
                        }
                        Err(e) => {
                            if !send(
                                &event_tx,
                                AgentEvent::Error {
                                    message: format!("Steering failed: {e}"),
                                },
                            )
                            .await
                            {
                                break 'main;
                            }
                        }
                    }
                }
                None => {
                    // Mailbox closed (the caller's graceful idle-reap): finish
                    // once the in-flight turn completes.
                    steering_open = false;
                    if !turn_active {
                        break 'main;
                    }
                }
            },

            _ = interrupt.cancelled(), if !interrupt_sent => {
                interrupt_sent = true;
                interrupted = true;
                if turn_active {
                    let client = client.clone();
                    let sid = session_id.clone();
                    tokio::spawn(async move {
                        if let Err(e) = client.abort(&sid).await {
                            tracing::debug!(target: "comet_harness::opencode", "abort failed (escalation will reap): {e}");
                        }
                    });
                    // Escalate if the server doesn't wind down (aborted error
                    // + idle) within the grace periods: SIGTERM, then SIGKILL.
                    if let Some(pid) = child.id() {
                        escalation = Some(tokio::spawn(async move {
                            tokio::time::sleep(interrupt_grace).await;
                            send_signal(pid, Signal::Term);
                            tokio::time::sleep(kill_grace).await;
                            send_signal(pid, Signal::Kill);
                        }));
                    }
                } else {
                    // Idle between turns: nothing to abort — the terminal
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
                    session_id: Some(session_id.clone()),
                }))
                .await;
        } else if !done_for_turn {
            // The stream ended before the turn settled. Distinguish a real
            // server exit from a clean stream end: a serve still running merely
            // closed its SSE feed (idle-parked or a one-shot subscription), so
            // the in-flight turn finishes Completed rather than reporting a
            // bogus "still running" crash. shutdown_child below reaps the
            // (possibly still-running, parked) serve we own either way.
            let error = match turn_error {
                // A turn error already booked keeps its own wording.
                Some(error) => Some(error),
                // The assistant message settled: Completed, unless the feed
                // ended because the serve is already gone.
                None if current_msg_completed => {
                    crash_if_exited(&mut child, &stderr_tail, Duration::ZERO).await
                }
                // A turn still streaming when the feed died never settled — a
                // stall, Errored (gh#37) — unless the feed died because the
                // serve did, in which case the crash is the real diagnosis and
                // we may be racing its exit, so wait for it first (gh#79).
                None => Some(
                    match crash_if_exited(&mut child, &stderr_tail, EXIT_PROBE_GRACE).await {
                        Some(crash) => crash,
                        None => STALLED_TURN.into(),
                    },
                ),
            };
            let _ = event_tx
                .send(Ok(AgentEvent::Done {
                    status: if error.is_some() {
                        DoneStatus::Errored
                    } else {
                        DoneStatus::Completed
                    },
                    result: None,
                    error,
                    session_id: Some(session_id.clone()),
                }))
                .await;
        }
    }

    shutdown_child(&mut child, kill_grace).await;
    if let Some(handle) = escalation {
        handle.abort();
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

    #[test]
    fn listening_port_is_parsed() {
        assert_eq!(
            parse_listening_port("opencode server listening on http://127.0.0.1:4096"),
            Some(4096)
        );
        assert_eq!(
            parse_listening_port("opencode server listening on http://127.0.0.1:54466"),
            Some(54466)
        );
        assert_eq!(parse_listening_port("some other log line"), None);
        assert_eq!(parse_listening_port("listening on http://127.0.0.1:ab12"), None);
    }

    #[test]
    fn model_ids_split_into_provider_and_model() {
        assert_eq!(
            split_model(&Some("deepseek/deepseek-v4-flash".into())),
            Some(("deepseek".into(), "deepseek-v4-flash".into()))
        );
        assert_eq!(
            split_model(&Some("big-pickle".into())),
            Some(("opencode".into(), "big-pickle".into()))
        );
        assert_eq!(split_model(&None), None);
    }

    #[test]
    fn mcp_servers_merge_into_opencode_inline_config() {
        let config = opencode_inline_config(
            Some(r#"{"theme":"nord","mcp":{"mine":{"type":"remote","url":"https://example.test","enabled":true}}}"#),
            &[comet_proto::McpServer {
                name: "comet-board".into(),
                command: "comet-board".into(),
                args: vec!["mcp".into()],
            }],
        )
        .unwrap()
        .expect("one server produces config");
        let config: serde_json::Value = serde_json::from_str(&config).unwrap();
        assert_eq!(config["theme"], "nord");
        assert_eq!(config["mcp"]["mine"]["type"], "remote");
        assert_eq!(
            config["mcp"]["comet-board"],
            serde_json::json!({
                "type": "local",
                "command": ["comet-board", "mcp"],
                "enabled": true,
            })
        );
        assert!(opencode_inline_config(None, &[]).unwrap().is_none());
    }
}
