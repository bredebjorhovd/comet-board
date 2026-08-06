//! The agent-facing half of `comet-board`: list / dispatch / retry / cancel /
//! wait / new — docs/BOARD.md §H6.
//!
//! Everything that reads or moves the live board goes through the engine's
//! typed RPC on the localhost IPC port, exactly as `comet-tui` attaches: the
//! engine owns `board.db` and the board loop, and `WatchBoard` streams the rows
//! it derives. The JSON these commands print is herdr-board's `list --json`
//! contract verbatim (modulo the pane→chat renames documented on
//! [`TaskRow`]) — the agent conventions text depends on that shape, so it is
//! not ours to bend here.
//!
//! The board those RPCs reach need not be this device's: they are all
//! relay-forwardable (gh#55), so [`Board`] carries a `targetDeviceId` and
//! `--device` points the whole CLI at the box that hosts the board (gh#73).
//!
//! `new` is the exception to "ask the engine": it writes to the *trackers*
//! (Linear / GitHub), which sit upstream of the engine, so it speaks to them
//! directly with the same clients the sync loop uses.

use anyhow::{Context, Result, anyhow, bail};
use comet_board::adopt::{git_remote, github_slug};
use comet_board::config::{self, Paths, RoutingConfig};
use comet_board::model::{BoardState, Source};
use comet_board::rows::TaskRow;
use comet_board::runtime::{RuntimeOption, harness_for_runtime, runtime_name};
use comet_proto::Device;
use comet_rpc::{RpcClient, connect_ws, methods};
use serde::Deserialize;
use std::time::Duration;

/// The engine answers a `WatchBoard` subscription with the current rows
/// immediately; anything slower than this is a listener that is not the engine.
pub const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(5);

/// Set in a board-dispatched chat's environment (H3), so `dispatch` from inside
/// one inherits identity the way `HERDR_PANE_ID` provided it under herdr.
pub const CHAT_ID_ENV: &str = "COMET_BOARD_CHAT_ID";

/// The engine connection plus which device's board it drives (gh#73).
///
/// The board store lives on exactly one device — usually the always-on box —
/// and every board RPC is relay-forwardable, so this dials the *local* engine
/// exactly as it always did and names the host in the params. That is the same
/// shape the desktop panel and the TUI pane send, and it is why `--device`
/// needs no new transport: the local engine forwards.
///
/// `host` is `None` for this device's own board, and then nothing extra is
/// sent — a single-device install speaks byte-for-byte what it spoke before.
pub struct Board {
    client: RpcClient,
    host: Option<String>,
}

/// Attach to the local engine, pointed at `device`'s board (`None` = this
/// device's own). A named device is resolved against the registered devices
/// first, so a typo costs an error naming the fleet rather than a call
/// forwarded into nothing.
pub async fn attach(port: u16, device: Option<&str>) -> Result<Board> {
    let client = connect_ws(&format!("ws://127.0.0.1:{port}"))
        .await
        .with_context(|| {
            format!(
                "connecting to the engine on 127.0.0.1:{port} — start `comet` or `comet headless`"
            )
        })?;
    let host = match device {
        Some(want) => Some(resolve_device(&client, want).await?),
        None => None,
    };
    Ok(Board { client, host })
}

impl Board {
    /// Merge the host's `targetDeviceId` passthrough into a call's params. The
    /// local board leaves them untouched.
    fn params(&self, value: serde_json::Value) -> serde_json::Value {
        with_target(value, self.host())
    }

    /// The device hosting the board, when it is not this one — for the error
    /// and confirmation text that has to say where the work landed.
    pub fn host(&self) -> Option<&str> {
        self.host.as_deref()
    }
}

/// The `targetDeviceId` passthrough merged into a call's params (gh#55). `None`
/// — the board is this device's — leaves the params untouched, so a
/// single-device install sends exactly the shape it always did.
fn with_target(value: serde_json::Value, host: Option<&str>) -> serde_json::Value {
    let mut value = value;
    if let (Some(host), Some(object)) = (host, value.as_object_mut()) {
        object.insert("targetDeviceId".into(), serde_json::json!(host));
    }
    value
}

/// A device id for `--device`: an exact id, or a name as the device switcher
/// shows it. Names are the ones people have, ids are the ones that are unique;
/// accepting both and refusing an ambiguous name is the only honest reading.
async fn resolve_device(client: &RpcClient, want: &str) -> Result<String> {
    let mut stream = client
        .subscribe(methods::WATCH_DEVICES, serde_json::json!({}))
        .await
        .context("listing devices")?;
    let first = tokio::time::timeout(SNAPSHOT_TIMEOUT, stream.recv())
        .await
        .map_err(|_| {
            anyhow!(
                "timed out after {}s listing devices",
                SNAPSHOT_TIMEOUT.as_secs()
            )
        })?
        .ok_or_else(|| anyhow!("the device stream ended before a snapshot"))?;
    let devices: Vec<Device> = serde_json::from_value(first).context("parsing devices")?;
    pick_device(&devices, want)
}

/// `--device`'s value against the registered devices: an id wins outright, then
/// a case-insensitive name. Two devices sharing a name is a real thing (two
/// laptops called `laptop`), and picking one would send the work somewhere the
/// operator did not choose — so that asks for the id instead.
fn pick_device(devices: &[Device], want: &str) -> Result<String> {
    if let Some(d) = devices.iter().find(|d| d.id == want) {
        return Ok(d.id.clone());
    }
    let named: Vec<&Device> = devices
        .iter()
        .filter(|d| d.name.eq_ignore_ascii_case(want))
        .collect();
    match named.as_slice() {
        [d] => Ok(d.id.clone()),
        [] => bail!("no device `{want}`; this org has: {}", device_list(devices)),
        several => bail!(
            "`{want}` names {} devices; use the id: {}",
            several.len(),
            several
                .iter()
                .map(|d| d.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn device_list(devices: &[Device]) -> String {
    if devices.is_empty() {
        return "none registered".to_string();
    }
    devices
        .iter()
        .map(|d| format!("{} ({})", d.name, d.id))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The first `WatchBoard` snapshot. A stream that ends before one arrives is a
/// reachable engine whose board is not running: the RPC layer folds the
/// server's error into end-of-stream, so name the likely cause here.
async fn snapshot(
    stream: &mut tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
    host: Option<&str>,
) -> Result<Vec<TaskRow>> {
    let first = tokio::time::timeout(SNAPSHOT_TIMEOUT, stream.recv())
        .await
        .map_err(|_| {
            anyhow!(
                "timed out after {}s waiting for board rows",
                SNAPSHOT_TIMEOUT.as_secs()
            )
        })?
        .ok_or_else(|| no_board(host))?;
    serde_json::from_value(first).context("parsing board rows")
}

/// "Said nothing at all" IS the answer: the engine refuses `WatchBoard` outright
/// when it hosts no board, and the RPC layer folds that refusal into
/// end-of-stream. Which device was asked decides what to suggest.
fn no_board(host: Option<&str>) -> anyhow::Error {
    match host {
        Some(device) => anyhow!(
            "device {device} hosts no board — its stream ended before a snapshot; \
             the board is disabled there (COMET_BOARD=0) or its service failed to start"
        ),
        None => anyhow!(
            "the board stream ended before a snapshot — this device's board is disabled \
             (COMET_BOARD=0) or its service failed to start (see the engine log); if the \
             board lives on another device, name it with --device"
        ),
    }
}

/// Current board rows, in board order — one snapshot of what `WatchBoard`
/// streams.
pub async fn board_rows(board: &Board) -> Result<Vec<TaskRow>> {
    let mut stream = board
        .client
        .subscribe(methods::WATCH_BOARD, board.params(serde_json::json!({})))
        .await?;
    snapshot(&mut stream, board.host()).await
}

// ---- list ---------------------------------------------------------------

/// Reject an unknown filter rather than returning an empty list: to a caller,
/// `[]` from a typo is indistinguishable from `[]` meaning "nothing is ready",
/// and the second is a normal answer worth acting on.
pub fn validate_filters(state: Option<&str>, source: Option<&str>) -> Result<()> {
    if let Some(want) = state
        && BoardState::parse(want).is_none()
    {
        bail!(
            "unknown state `{want}`; expected one of: {}",
            state_names().join(", ")
        );
    }
    if let Some(want) = source
        && Source::parse(want).is_none()
    {
        bail!("unknown source `{want}`; expected linear or github");
    }
    Ok(())
}

fn state_names() -> Vec<&'static str> {
    BoardState::SECTION_ORDER
        .iter()
        .map(|s| s.as_str())
        .collect()
}

pub fn filter_rows(rows: Vec<TaskRow>, state: Option<&str>, source: Option<&str>) -> Vec<TaskRow> {
    rows.into_iter()
        .filter(|r| state.is_none_or(|want| r.state == want))
        .filter(|r| source.is_none_or(|want| r.source == want))
        .collect()
}

pub fn print_tasks(rows: &[TaskRow], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("nothing on the board");
        return Ok(());
    }
    for r in rows {
        let extra = match (&r.pr_url, r.dispatchable) {
            // Two different reasons a row cannot be dispatched, and calling the
            // second one "no route" would send you to routing.toml for nothing.
            _ if r.gone => "  (gone upstream)".to_string(),
            (Some(pr), _) => format!("  {pr}"),
            (None, false) => "  (no route)".to_string(),
            _ => String::new(),
        };
        println!(
            "{:<8} {:<24} {:<10} {}{}",
            r.state,
            r.id,
            r.workspace.as_deref().unwrap_or("-"),
            truncate(&r.title, 48),
            extra
        );
    }
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

// ---- dispatch / retry / cancel ------------------------------------------

/// What `DispatchTask` answers: the attempt's address.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Dispatched {
    pub chat_id: String,
    pub cwd: String,
    pub attempt: usize,
}

/// Everything a release carries besides the task itself.
#[derive(Debug, Default, Clone, Copy)]
pub struct DispatchOpts<'a> {
    /// The dispatching chat's id — provenance, never authority.
    pub via: Option<&'a str>,
    /// Runtime override for this one dispatch; `None` = the route's.
    pub runtime: Option<&'a str>,
    /// Model override for the chosen harness.
    pub model: Option<&'a str>,
    /// Agent-account slot whose subscription this run spends.
    pub account: Option<&'a str>,
    /// End the task's live attempt and release a fresh one — `retry` on a
    /// blocked row (gh#49), and the one deliberate breach of the
    /// one-live-attempt rule. Ordinary dispatches send `false` and are refused
    /// on a live attempt.
    pub replace: bool,
}

/// Whether `retry` on a row in this state has to replace a live attempt.
///
/// Only `blocked`: its agent is alive and waiting on input, so the
/// one-live-attempt rule would refuse a plain dispatch, and ending it is the
/// whole point of retrying. A `failed` attempt is already closed — replacing
/// nothing would be a lie in the log — so that one is an ordinary release, and
/// so is a `ready` row that never got off the ground. Anything else keeps its
/// live attempt and is left to the engine's own refusal, which names the chat.
///
/// Exactly the desktop panel's rule (`crates/ui/src/board.rs`), so the same row
/// retried from a shell and from the panel takes the same path.
pub fn retry_replaces(state: &str) -> bool {
    state == BoardState::Blocked.as_str()
}

/// The dispatching chat's id: `--via` when given, else the identity a
/// board-dispatched chat inherits from its environment. Provenance, never
/// authority — the engine records it on the attempt and nothing else.
pub fn provenance(flag: Option<String>) -> Option<String> {
    provenance_from(flag, std::env::var(CHAT_ID_ENV).ok())
}

fn provenance_from(flag: Option<String>, env: Option<String>) -> Option<String> {
    flag.or(env).filter(|s| !s.is_empty())
}

/// One model a dispatch can be pointed at, as `ListModels` reports it for a
/// harness — `id` is exactly what the `DispatchTask` override sends, so it is
/// the only field a check can be made against.
#[derive(Debug, Deserialize)]
struct ModelChoice {
    id: String,
}

/// `dispatch`, with `--runtime` / `--model` checked against the engine's own
/// catalogs first — the same two calls the desktop picker fills its rows from
/// (`ListBoardRuntimes`, `ListModels { harness }`), so the CLI refuses exactly
/// what the picker would not have offered.
///
/// Worth a round-trip before the verb: the engine rejects an unknown *runtime*
/// on its own, but an unknown *model* is the harness's business, and by the
/// time the harness sees it the dispatch has already cut a worktree, made a
/// chat and started an agent. A typo should cost an error, not an attempt.
pub async fn dispatch_checked(
    board: &Board,
    task_id: &str,
    opts: DispatchOpts<'_>,
) -> Result<Dispatched> {
    // A model with no runtime beside it runs under the row's — which is what
    // `list --json` shows as that row's `runtime`, and what `--runtime`'s help
    // points at. Only fetched when it is the missing half of the answer.
    let row_runtime = match (opts.model, opts.runtime) {
        (Some(_), None) => board_rows(board)
            .await
            .ok()
            .and_then(|rows| rows.into_iter().find(|r| r.id == task_id))
            .and_then(|r| r.runtime),
        _ => None,
    };
    check_overrides(board, opts.runtime, opts.model, row_runtime.as_deref()).await?;
    dispatch(board, task_id, opts).await
}

/// `retry`, deciding from the row itself whether the release has to replace a
/// live attempt (gh#73). Reading the row is not optional: `replace` is a real
/// cancellation, and sending it unconditionally would let `retry` end a
/// *working* agent that nobody asked to interrupt.
pub async fn retry(board: &Board, task_id: &str, opts: DispatchOpts<'_>) -> Result<Retried> {
    let row = board_rows(board)
        .await?
        .into_iter()
        .find(|r| r.id == task_id)
        .ok_or_else(|| anyhow!("{task_id} is not on the board"))?;
    let replace = retry_replaces(&row.state);
    let dispatched = dispatch_checked(board, task_id, DispatchOpts { replace, ..opts }).await?;
    Ok(Retried {
        dispatched,
        replaced: replace,
        was: row.state,
    })
}

/// What `retry` did, so the confirmation can say whether an attempt was ended
/// to make room — the difference between a retry and a first dispatch is the
/// agent that was killed, and that is worth one line.
pub struct Retried {
    pub dispatched: Dispatched,
    pub replaced: bool,
    /// The state the row was in when the retry was decided.
    pub was: String,
}

/// Refuse an override the dispatch would only choke on later.
///
/// A catalog that cannot be read proves nothing either way, so it degrades to
/// a note on stderr and lets the dispatch through — the same choice the desktop
/// picker makes when `ListBoardRuntimes` fails (it dispatches with the route's
/// rather than trapping the operator). Refusing there would ground a legitimate
/// dispatch on the strength of a failed lookup.
async fn check_overrides(
    board: &Board,
    runtime: Option<&str>,
    model: Option<&str>,
    row_runtime: Option<&str>,
) -> Result<()> {
    if runtime.is_none() && model.is_none() {
        return Ok(());
    }

    let mut chosen = runtime.or(row_runtime).map(str::to_string);
    if let Some(want) = runtime {
        match board
            .client
            .call(
                methods::LIST_BOARD_RUNTIMES,
                board.params(serde_json::json!({})),
            )
            .await
            .context("listing runtimes")
            .and_then(|v| {
                serde_json::from_value::<Vec<RuntimeOption>>(v).context("parsing runtimes")
            }) {
            Ok(options) => chosen = Some(resolve_runtime(&options, want)?),
            Err(e) => eprintln!(
                "note: could not list runtimes ({e:#}) — dispatching with `{want}` unchecked"
            ),
        }
    }

    let Some(want) = model else { return Ok(()) };
    // Which harness's catalog to ask about. `ListModels` is keyed by harness
    // id, and the canonical runtime name *is* that id — an alias spelling
    // (`claude`, `openai-codex`) has to be resolved through the same table the
    // engine's own `build_spec` uses, or the lookup asks about nothing. It
    // carries the host too: the run executes there, so the catalog a dispatch
    // is checked against has to be that device's.
    let Some(harness) = chosen
        .as_deref()
        .and_then(harness_for_runtime)
        .map(runtime_name)
    else {
        // No runtime to attribute the model to: an unrouted row, or one whose
        // runtime is not a comet harness. The dispatch itself will say so.
        return Ok(());
    };

    match board
        .client
        .call(
            methods::LIST_MODELS,
            board.params(serde_json::json!({ "harness": harness })),
        )
        .await
        .context("listing models")
        .and_then(|v| serde_json::from_value::<Vec<ModelChoice>>(v).context("parsing models"))
    {
        Ok(models) => check_model(&models, want, harness),
        Err(e) => {
            eprintln!(
                "note: could not list {harness} models ({e:#}) — dispatching with `{want}` unchecked"
            );
            Ok(())
        }
    }
}

/// The canonical runtime name for an override, or an error naming what the
/// engine does offer.
fn resolve_runtime(options: &[RuntimeOption], want: &str) -> Result<String> {
    // The catalog offers one canonical name per harness; `routing.toml`'s alias
    // spellings are valid input the picker has no reason to list twice, so an
    // alias resolves to the canonical entry rather than being refused.
    let canonical = harness_for_runtime(want).map(runtime_name);
    options
        .iter()
        .find(|o| o.name.eq_ignore_ascii_case(want) || Some(o.name.as_str()) == canonical)
        .map(|o| o.name.clone())
        .ok_or_else(|| {
            anyhow!(
                "unknown runtime `{want}`; the engine offers: {}",
                options
                    .iter()
                    .map(|o| o.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

fn check_model(models: &[ModelChoice], want: &str, runtime: &str) -> Result<()> {
    // An empty catalog is a harness that could not enumerate (opencode with no
    // provider reachable, say), not a harness with no models — nothing to
    // check the override against.
    if models.is_empty() || models.iter().any(|m| m.id.eq_ignore_ascii_case(want)) {
        return Ok(());
    }
    bail!(
        "`{runtime}` has no model `{want}`; its catalog: {}",
        models
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

pub async fn dispatch(board: &Board, task_id: &str, opts: DispatchOpts<'_>) -> Result<Dispatched> {
    let params = board.params(dispatch_params(task_id, opts));
    let reply = board.client.call(methods::DISPATCH_TASK, params).await?;
    serde_json::from_value(reply).context("parsing DispatchTask reply")
}

/// The `DispatchTask` params for a release. Only the overrides that were given
/// are sent: an absent key is "the route's", which is not the same statement as
/// a null, and `replace` is absent unless it is true so an ordinary dispatch
/// sends what it always sent.
fn dispatch_params(task_id: &str, opts: DispatchOpts<'_>) -> serde_json::Value {
    let mut params = serde_json::json!({ "taskId": task_id, "via": opts.via });
    let Some(object) = params.as_object_mut() else {
        return params;
    };
    for (key, value) in [
        ("runtime", opts.runtime),
        ("model", opts.model),
        ("account", opts.account),
    ] {
        if let Some(value) = value {
            object.insert(key.into(), serde_json::Value::String(value.to_string()));
        }
    }
    if opts.replace {
        object.insert("replace".into(), serde_json::Value::Bool(true));
    }
    params
}

pub async fn cancel(board: &Board, task_id: &str) -> Result<()> {
    board
        .client
        .call(
            methods::CANCEL_TASK,
            board.params(serde_json::json!({ "taskId": task_id })),
        )
        .await?;
    Ok(())
}

// ---- wait ---------------------------------------------------------------

/// The states that count as settled for one `wait` call.
///
/// The default trio is "finished, one way or another": work to look at, work
/// that broke, or work whose ticket closed under it. `blocked` is deliberately
/// not among them — an agent pausing for an approval mid-run is not a result,
/// and an orchestrator that waited on it would be woken by every permission
/// prompt.
///
/// But a child that asks a question and is never answered never settles either,
/// and `wait` would then hold until its timeout or forever (gh#73). So
/// `--blocked-is-settled` *adds* blocked to whichever set is in play rather
/// than replacing it: the caller wants to be called back when its child needs
/// an answer AND when it finishes, and spelling the whole set out by hand to
/// say that is the kind of thing nobody does twice.
pub fn settle_states(explicit: &[String], blocked_is_settled: bool) -> Vec<String> {
    let mut states = if explicit.is_empty() {
        vec![
            BoardState::Review.as_str().to_string(),
            BoardState::Failed.as_str().to_string(),
            BoardState::Done.as_str().to_string(),
        ]
    } else {
        explicit.to_vec()
    };
    let blocked = BoardState::Blocked.as_str();
    if blocked_is_settled && !states.iter().any(|s| s == blocked) {
        states.push(blocked.to_string());
    }
    states
}

/// Block until watched work settles — the counterpart to `dispatch`, so an
/// orchestrator can release work and be told, instead of polling or falling
/// silent until a human prods it.
///
/// herdr-board's `wait` was a poll loop that reconciled as it went; here it is
/// a `WatchBoard` subscription, which is the same promise kept better — the
/// engine pushes rows after every sync cycle, status refresh and command, so
/// this answers as soon as the answer is true without doing any work itself.
pub async fn wait_for(
    board: &Board,
    tasks: &[String],
    states: &[String],
    timeout: Option<Duration>,
) -> Result<Vec<TaskRow>> {
    for state in states {
        if BoardState::parse(state).is_none() {
            bail!(
                "unknown state `{state}`; expected one of: {}",
                state_names().join(", ")
            );
        }
    }
    let started = tokio::time::Instant::now();
    let deadline = timeout.map(|t| started + t);

    let mut stream = board
        .client
        .subscribe(methods::WATCH_BOARD, board.params(serde_json::json!({})))
        .await?;
    let mut rows = snapshot(&mut stream, board.host()).await?;

    // With no explicit tasks, watch whatever is in flight right now. Resolved
    // once, at the start: a task dispatched later is not what this call is
    // waiting for.
    let watching: Vec<String> = if tasks.is_empty() {
        rows.iter()
            .filter(|r| in_flight(r))
            .map(|r| r.id.clone())
            .collect()
    } else {
        tasks.to_vec()
    };
    if watching.is_empty() {
        // Distinct from "nothing matched": there was never anything to wait
        // for, which usually means the caller dispatched nothing, or the work
        // had already settled before it asked.
        bail!("nothing is in flight to wait for");
    }

    loop {
        let matched = settled(&rows, &watching, states);
        if !matched.is_empty() {
            return Ok(matched);
        }
        let next = match deadline {
            Some(d) => tokio::time::timeout_at(d, stream.recv())
                .await
                .map_err(|_| {
                    anyhow!(
                        "timed out after {:?} waiting for {} task(s) to reach {states:?}",
                        started.elapsed(),
                        watching.len()
                    )
                })?,
            None => stream.recv().await,
        };
        rows = match next {
            Some(v) => serde_json::from_value(v).context("parsing board rows")?,
            None => bail!("the board stream ended while waiting — did the engine stop?"),
        };
    }
}

/// A row with a live attempt. `chat_id` is set exactly when a live attempt has
/// its chat; `working`/`blocked` cover the window where the attempt exists but
/// its status has not folded into the row yet.
fn in_flight(r: &TaskRow) -> bool {
    r.chat_id.is_some() || matches!(r.state.as_str(), "working" | "blocked")
}

fn settled(rows: &[TaskRow], watching: &[String], states: &[String]) -> Vec<TaskRow> {
    rows.iter()
        .filter(|r| watching.contains(&r.id) && states.contains(&r.state))
        .cloned()
        .collect()
}

/// Wait for a task to appear on the board — `new --dispatch` needs the row to
/// exist before it can be released, and the engine's sync loop is what puts it
/// there on its next poll.
pub async fn await_row(board: &Board, task_id: &str, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut stream = board
        .client
        .subscribe(methods::WATCH_BOARD, board.params(serde_json::json!({})))
        .await?;
    let mut rows = snapshot(&mut stream, board.host()).await?;
    loop {
        if rows.iter().any(|r| r.id == task_id) {
            return Ok(());
        }
        let next = tokio::time::timeout_at(deadline, stream.recv())
            .await
            .map_err(|_| {
                anyhow!(
                    "{task_id} did not reach the board within {}s — it exists upstream; \
                     dispatch it once it appears: comet-board dispatch --task {task_id}",
                    timeout.as_secs()
                )
            })?;
        rows = match next {
            Some(v) => serde_json::from_value(v).context("parsing board rows")?,
            None => bail!("the board stream ended while waiting — did the engine stop?"),
        };
    }
}

// ---- new ----------------------------------------------------------------

/// Where a new ticket should be written.
///
/// There is no inferring this. A label maps to a *route* — which repo the work
/// happens in — and says nothing about which tracker the project's tickets live
/// in. This board's own backlog is in Linear while its code is on GitHub, and a
/// repo whose issues you already keep on GitHub is the reverse, so the same
/// label would justify either answer. `[defaults] new_source` sets the habit;
/// `--source` overrides it.
#[derive(Debug, Default)]
pub struct NewTask<'a> {
    pub title: &'a str,
    pub body: Option<&'a str>,
    /// Linear team key. Only needed with more than one team.
    pub team: Option<&'a str>,
    pub labels: &'a [String],
    /// `linear` or `github`; falls back to `[defaults] new_source`.
    pub source: Option<&'a str>,
    /// `owner/repo`, for GitHub with several repos configured.
    pub repo: Option<&'a str>,
}

/// Write a ticket, so recording the work is cheaper than not recording it.
///
/// Work that goes through a ticket is traceable — reasoning, branch, PR,
/// review, closure. Work that does not is a wall of commits somebody has to
/// reconstruct later. The difference in practice is almost entirely friction,
/// so this exists to remove it.
pub fn new_task(
    paths: &Paths,
    cfg: &RoutingConfig,
    spec: &NewTask<'_>,
) -> Result<(String, String)> {
    let source = spec
        .source
        .map(str::to_string)
        .unwrap_or_else(|| cfg.defaults.new_source.clone());

    if source == "github" {
        let here = git_remote(".").as_deref().and_then(github_slug);
        let repo = github_repo(&cfg.github.repos, spec.repo, here)?;
        if matches!(config::github_auth(paths), config::GithubAuth::None) {
            bail!(
                "no GitHub credential — set GITHUB_TOKEN, or GITHUB_APP_ID and \
                 GITHUB_APP_PRIVATE_KEY_PATH; see `comet-board doctor`"
            );
        }
        let gh = comet_board::sources::github::Github::new(
            comet_board::sources::github::HttpRest::from_paths(paths)?,
        );
        let (number, url) = gh.create_issue(&repo, spec.title, spec.body, spec.labels)?;
        return Ok((format!("{repo}#{number}"), url));
    }
    if source != "linear" {
        bail!("unknown source `{source}`; expected linear or github");
    }

    let key = config::linear_api_key(paths)
        .ok_or_else(|| anyhow!("no LINEAR_API_KEY; see `comet-board doctor`"))?;
    let linear = comet_board::sources::linear::Linear::new(
        comet_board::sources::linear::HttpTransport::new(key)?,
    );

    let teams = linear.team_ids()?;
    let team_id = match spec.team {
        Some(k) => teams
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(k))
            .map(|(_, id)| id.clone())
            .ok_or_else(|| {
                anyhow!(
                    "no Linear team `{k}`; known: {}",
                    teams
                        .iter()
                        .map(|(key, _)| key.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?,
        // With one team there is nothing to choose; with several, say so rather
        // than filing into whichever came back first.
        None if teams.len() == 1 => teams[0].1.clone(),
        None => bail!(
            "several Linear teams exist; name one with --team: {}",
            teams
                .iter()
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };

    let known = linear.label_ids().unwrap_or_default();
    let mut ids = Vec::new();
    for want in spec.labels {
        match known
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(want))
        {
            Some((_, id)) => ids.push(id.clone()),
            None => bail!(
                "no Linear label `{want}`; known: {}",
                known
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    linear.create_issue(&team_id, spec.title, spec.body, &ids)
}

/// Which repo a GitHub ticket goes to, in order of explicitness: `--repo`, the
/// only configured repo, the checkout you are standing in.
///
/// Standing in a repo is a statement about which repo you mean — an agent
/// filing a ticket is almost always inside the checkout the ticket is about,
/// and making it pass `--repo` is asking it to repeat what the working
/// directory already says. But only a repo the board actually polls: filing
/// into an unpolled one writes a real issue that never reaches the board, which
/// is the failure this would otherwise cause silently and often.
fn github_repo(repos: &[String], flag: Option<&str>, here: Option<String>) -> Result<String> {
    flag.map(str::to_string)
        // One configured repo is not a choice; several are.
        .or_else(|| (repos.len() == 1).then(|| repos[0].clone()))
        .or_else(|| {
            let slug = here.clone()?;
            repos
                .iter()
                .find(|r| r.eq_ignore_ascii_case(&slug))
                .cloned()
        })
        .ok_or_else(|| match here {
            // In a GitHub repo, but not one the board watches. Naming the repo
            // would not help: the issue still would not show up. Adopting it is
            // the fix.
            Some(slug) => anyhow!(
                "{slug} is not polled by the board, so a ticket filed there would not \
                 appear — adopt it first (`comet-board adopt {slug}`), or name another \
                 with --repo; configured: {}",
                repos.join(", ")
            ),
            None => anyhow!(
                "name the repo with --repo; configured: {}",
                repos.join(", ")
            ),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, state: &str) -> TaskRow {
        TaskRow {
            id: id.into(),
            identifier: id.into(),
            title: format!("task {id}"),
            state: state.into(),
            source: "github".into(),
            url: String::new(),
            labels: vec![],
            dispatchable: true,
            gone: false,
            route: None,
            workspace: None,
            runtime: None,
            chat_id: None,
            pr_url: None,
            pr_number: None,
            branch: None,
            dispatched_by: None,
            dispatched_by_chat: None,
            last_outcome: None,
            last_outcome_at: None,
            attempts: 0,
            reopened: 0,
            updated_at: "2026-08-01T11:00:00Z".into(),
            started_at: None,
            account: None,
            dispatched_by_user: None,
        }
    }

    #[test]
    fn unknown_filters_are_rejected_not_emptied() {
        // `[]` from a typo would be indistinguishable from "nothing is ready".
        assert!(validate_filters(Some("redy"), None).is_err());
        assert!(validate_filters(None, Some("jira")).is_err());
        assert!(validate_filters(Some("ready"), Some("linear")).is_ok());
        assert!(validate_filters(None, None).is_ok());
    }

    #[test]
    fn filters_apply_by_state_and_source() {
        let mut linear_row = row("linear:AGE-1", "working");
        linear_row.source = "linear".into();
        let rows = vec![
            row("gh:o/r#1", "ready"),
            row("gh:o/r#2", "working"),
            linear_row,
        ];

        let ready = filter_rows(rows.clone(), Some("ready"), None);
        assert_eq!(
            ready.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["gh:o/r#1"]
        );

        let linear = filter_rows(rows, None, Some("linear"));
        assert_eq!(
            linear.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["linear:AGE-1"]
        );
    }

    #[test]
    fn in_flight_means_a_live_attempt_not_a_state_name() {
        let mut with_chat = row("a", "ready");
        with_chat.chat_id = Some("chat-1".into());
        assert!(
            in_flight(&with_chat),
            "a chat holds a live attempt whatever the derived state"
        );
        assert!(in_flight(&row("b", "working")));
        assert!(in_flight(&row("c", "blocked")));
        assert!(!in_flight(&row("d", "ready")));
        assert!(!in_flight(&row("e", "review")));
    }

    #[test]
    fn settled_matches_only_watched_tasks_in_wanted_states() {
        let rows = vec![row("a", "review"), row("b", "review"), row("c", "working")];
        let watching = vec!["a".to_string(), "c".into()];
        let states = vec!["review".to_string(), "failed".into(), "done".into()];
        let matched = settled(&rows, &watching, &states);
        assert_eq!(
            matched.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["a"]
        );
    }

    #[test]
    fn provenance_prefers_the_flag_and_ignores_empties() {
        assert_eq!(
            provenance_from(Some("chat-flag".into()), Some("chat-env".into())).as_deref(),
            Some("chat-flag")
        );
        assert_eq!(
            provenance_from(None, Some("chat-env".into())).as_deref(),
            Some("chat-env")
        );
        assert_eq!(provenance_from(None, Some(String::new())), None);
        assert_eq!(provenance_from(None, None), None);
    }

    fn runtimes() -> Vec<RuntimeOption> {
        comet_board::runtime::runtime_options()
    }

    fn models(ids: &[&str]) -> Vec<ModelChoice> {
        ids.iter()
            .map(|id| ModelChoice { id: (*id).into() })
            .collect()
    }

    #[test]
    fn runtime_override_resolves_aliases_and_refuses_the_rest() {
        // Canonical, as the picker offers it.
        assert_eq!(
            resolve_runtime(&runtimes(), "opencode").unwrap(),
            "opencode"
        );
        // `routing.toml` spellings the engine accepts but the catalog does not
        // list twice — refusing these would refuse what a route already says.
        assert_eq!(
            resolve_runtime(&runtimes(), "claude").unwrap(),
            "claude-code"
        );
        assert_eq!(
            resolve_runtime(&runtimes(), "openai-codex").unwrap(),
            "codex"
        );

        let err = resolve_runtime(&runtimes(), "claude-cod")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown runtime `claude-cod`"), "{err}");
        // The error has to carry the answer; there is no picker to fall back on.
        assert!(err.contains("claude-code"), "{err}");
    }

    #[test]
    fn model_override_is_checked_against_the_catalog() {
        let catalog = models(&["claude-opus-5", "claude-sonnet-5"]);
        assert!(check_model(&catalog, "claude-opus-5", "claude-code").is_ok());

        let err = check_model(&catalog, "opus", "claude-code")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no model `opus`"), "{err}");
        assert!(err.contains("claude-opus-5"), "{err}");

        // A harness that could not enumerate proves nothing — the override
        // stands and the harness stays the authority.
        assert!(check_model(&[], "anything", "opencode").is_ok());
    }

    #[test]
    fn github_repo_resolution_in_order_of_explicitness() {
        let repos = vec!["owner/one".to_string(), "owner/two".into()];
        // The flag wins.
        assert_eq!(
            github_repo(&repos, Some("owner/two"), Some("owner/one".into())).unwrap(),
            "owner/two"
        );
        // One configured repo is not a choice.
        assert_eq!(
            github_repo(&["owner/only".to_string()], None, None).unwrap(),
            "owner/only"
        );
        // The checkout you are standing in, but only if the board polls it.
        assert_eq!(
            github_repo(&repos, None, Some("Owner/One".into())).unwrap(),
            "owner/one"
        );
        let err = github_repo(&repos, None, Some("owner/unpolled".into())).unwrap_err();
        assert!(err.to_string().contains("adopt"), "{err}");
        assert!(github_repo(&repos, None, None).is_err());
    }

    fn device(id: &str, name: &str) -> Device {
        Device {
            id: id.into(),
            name: name.into(),
            platform: "linux".into(),
            last_seen_at: None,
            created_at: None,
            version: None,
        }
    }

    #[test]
    fn device_resolves_by_id_then_name() {
        let devices = vec![device("dev-1", "the-box"), device("dev-2", "laptop")];
        assert_eq!(pick_device(&devices, "dev-2").unwrap(), "dev-2");
        // The name people actually have, spelled how they feel like spelling it.
        assert_eq!(pick_device(&devices, "The-Box").unwrap(), "dev-1");

        let err = pick_device(&devices, "boxx").unwrap_err().to_string();
        assert!(err.contains("no device `boxx`"), "{err}");
        // The error carries the fleet; there is no picker to fall back on.
        assert!(err.contains("the-box (dev-1)"), "{err}");
    }

    #[test]
    fn an_ambiguous_device_name_asks_for_the_id() {
        // Two laptops called `laptop` is a real fleet. Picking one would send
        // the work to a device the operator did not choose.
        let devices = vec![device("dev-1", "laptop"), device("dev-2", "laptop")];
        let err = pick_device(&devices, "laptop").unwrap_err().to_string();
        assert!(err.contains("names 2 devices"), "{err}");
        assert!(err.contains("dev-1, dev-2"), "{err}");

        // An id is never ambiguous, even when the names collide.
        assert_eq!(pick_device(&devices, "dev-2").unwrap(), "dev-2");
    }

    #[test]
    fn the_host_rides_on_board_calls_and_a_local_board_sends_nothing_extra() {
        assert_eq!(
            with_target(serde_json::json!({ "taskId": "t" }), Some("dev-1")),
            serde_json::json!({ "taskId": "t", "targetDeviceId": "dev-1" })
        );
        assert_eq!(
            with_target(serde_json::json!({ "taskId": "t" }), None),
            serde_json::json!({ "taskId": "t" })
        );
    }

    #[test]
    fn dispatch_sends_only_the_overrides_it_was_given() {
        // An absent key is "the route's", which is not the same statement as a
        // null — and `replace` absent is what an ordinary dispatch has always
        // sent.
        assert_eq!(
            dispatch_params("gh:o/r#1", DispatchOpts::default()),
            serde_json::json!({ "taskId": "gh:o/r#1", "via": null })
        );
        assert_eq!(
            dispatch_params(
                "gh:o/r#1",
                DispatchOpts {
                    via: Some("chat-7"),
                    runtime: Some("opencode"),
                    model: Some("claude-opus-5"),
                    account: Some("slot-a"),
                    replace: true,
                }
            ),
            serde_json::json!({
                "taskId": "gh:o/r#1",
                "via": "chat-7",
                "runtime": "opencode",
                "model": "claude-opus-5",
                "account": "slot-a",
                "replace": true,
            })
        );
    }

    #[test]
    fn retry_replaces_only_a_blocked_rows_live_attempt() {
        // Blocked: the agent is alive and waiting, so the one-live-attempt rule
        // would refuse a plain dispatch — ending it is the point.
        assert!(retry_replaces("blocked"));
        // Failed and ready hold nothing live; replacing nothing would be a lie
        // in the log, and the engine's own guard is free to refuse the rest.
        assert!(!retry_replaces("failed"));
        assert!(!retry_replaces("ready"));
        assert!(!retry_replaces("working"));
        assert!(!retry_replaces("review"));
        assert!(!retry_replaces("done"));
    }

    #[test]
    fn blocked_is_settled_adds_to_the_states_rather_than_replacing_them() {
        assert_eq!(settle_states(&[], false), ["review", "failed", "done"]);
        // The orchestrator wants to hear about a question AND about the finish.
        assert_eq!(
            settle_states(&[], true),
            ["review", "failed", "done", "blocked"]
        );
        // An explicit set is still the caller's; the flag only tops it up.
        assert_eq!(
            settle_states(&["done".to_string()], true),
            ["done", "blocked"]
        );
        // And it never doubles a state the caller already named.
        assert_eq!(settle_states(&["blocked".to_string()], true), ["blocked"]);
    }

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate("short", 48), "short");
        assert_eq!(truncate("ålesund", 4), "åle…");
    }
}
