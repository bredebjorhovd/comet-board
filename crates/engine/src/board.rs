//! The board service — the engine hosting what herdr-board's `syncd` ran as a
//! separate daemon (docs/BOARD.md §H1), plus the dispatch/cancel verbs and the
//! `WatchBoard` rows feed the RPC surface serves (§H2).
//!
//! Three inputs drive the board:
//!
//! - **The sync interval** (`[sync] interval` in `routing.toml`, default 30s):
//!   poll Linear/GitHub, reconcile session state, derive states, drain the
//!   writeback queue, deliver review comments (§H5) against the pulls the
//!   cycle just polled. This is the loop with lifecycle authority — orphaning
//!   rides its steady clock, so a burst of events cannot age an attempt faster
//!   than wall time.
//! - **The session watch** (the same merged stream `WatchSessions` serves):
//!   each snapshot is mapped through `comet_board::runtime::agent_status` and
//!   written onto live attempts as a status-only refresh, so a `blocked` agent
//!   is visible on the board the moment it asks, not on the next tick.
//! - **Commands** (`DispatchTask` / `CancelTask`): executed on the loop thread,
//!   because `board.db` has one writer and it is this one. Dispatch resolution
//!   is `comet_board::dispatch`; execution is the [`Runtime`] — H3 grows the
//!   pipeline (concurrency caps, dispatcher provenance) around the same seam.
//!
//! The sources are blocking HTTP and SQLite wants one writer, so everything
//! runs on a dedicated thread; the async side only forwards watch snapshots
//! into its channel. Disable with `COMET_BOARD=0` — with no `routing.toml` and
//! no credentials the loop polls nothing and the idle cost is one thread and a
//! SQLite handle.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use comet_board::config::Paths;
use comet_board::db::NewAttempt;
use comet_board::dispatch::{
    DispatchOrigin, DispatchOverrides, SpaceRef, build_spec, check_capacity, dispatcher_for,
    dispatcher_name, route_for, space_matches,
};
use comet_board::log::Logger;
use comet_board::model::{AgentStatus, Outcome};
use comet_board::rows::{TaskDetail, TaskRow, board_rows, task_detail};
use comet_board::runtime::{Runtime, agent_status};
use comet_board::stats::Stats as BoardStats;
use comet_board::sync::{SessionStatuses, SyncEngine};
use comet_proto::view::board::OrchestratorPin;
use comet_proto::{Session, Space};
use tokio::sync::{oneshot, watch};

/// What a dispatch returns to its caller: the attempt's address.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Dispatched {
    pub chat_id: String,
    pub cwd: String,
    pub attempt: usize,
}

enum Msg {
    Sessions(Vec<Session>),
    Dispatch {
        task_id: String,
        origin: DispatchOrigin,
        overrides: DispatchOverrides,
        /// End the task's live attempt and release a fresh one — the blocked
        /// row's Retry (gh#49), the deliberate exception to the one-live-attempt
        /// rule. Plain dispatches send `false` and are refused on a live attempt.
        replace: bool,
        reply: oneshot::Sender<anyhow::Result<Dispatched>>,
    },
    Cancel {
        task_id: String,
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
    /// Read one task's issue text (gh#132). On the loop's thread like every
    /// other read, because that thread owns `board.db` — a second connection
    /// opened per detail fetch would be a second owner of the store.
    Detail {
        task_id: String,
        reply: oneshot::Sender<anyhow::Result<TaskDetail>>,
    },
    /// Throughput over a window (gh#143). On the loop's thread for the same
    /// reason `Detail` is: that thread owns `board.db`.
    Stats {
        since_days: Option<i64>,
        reply: oneshot::Sender<anyhow::Result<BoardStats>>,
    },
    Shutdown,
}

/// Handle to the running board loop. Owned by the engine core; the RPC surface
/// (`WatchBoard` / `DispatchTask` / `CancelTask`) is served off it.
pub struct BoardService {
    tx: mpsc::Sender<Msg>,
    thread: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    watch_task: tokio::task::JoinHandle<()>,
    rows: watch::Receiver<Vec<TaskRow>>,
    /// The pinned orchestrator (gh#104). Held as the *sender* so two writers
    /// can reach it: the loop, which republishes whenever it rereads
    /// `routing.toml` (an `$EDITOR` over ssh), and
    /// [`BoardService::note_config`], which republishes the instant a
    /// `WriteBoardConfig` lands — pinning a chat from the app is a direct
    /// action, and a glyph that appeared up to a sync interval later would read
    /// as the click not having worked.
    orchestrator: Arc<watch::Sender<OrchestratorPin>>,
    /// Where this board's `routing.toml`, `.env` and store live. Kept here so
    /// the config RPCs (gh#75) resolve them off the running service rather than
    /// re-deriving them from the data dir — two answers to "which routing.toml"
    /// is one too many, and `COMET_BOARD_CONFIG_DIR` makes it a real risk.
    paths: Paths,
}

impl BoardService {
    /// Open the board under the engine's data dir and start the loop. Must be
    /// called from a tokio runtime (the session-watch forwarder is a task);
    /// `handle` is the runtime the loop enters so dispatch's `tokio::spawn` /
    /// `Handle::block_on` work from its plain thread.
    pub fn spawn(
        data_dir: &std::path::Path,
        sessions: watch::Receiver<Vec<Session>>,
        runtime: Arc<dyn Runtime + Send + Sync>,
        spaces: watch::Receiver<Vec<Space>>,
        handle: tokio::runtime::Handle,
    ) -> anyhow::Result<BoardService> {
        Self::spawn_at(Paths::under(data_dir)?, sessions, runtime, spaces, handle)
    }

    /// As [`BoardService::spawn`], with the directories chosen by the caller —
    /// the seam the tests use.
    pub fn spawn_at(
        paths: Paths,
        mut sessions: watch::Receiver<Vec<Session>>,
        runtime: Arc<dyn Runtime + Send + Sync>,
        spaces: watch::Receiver<Vec<Space>>,
        handle: tokio::runtime::Handle,
    ) -> anyhow::Result<BoardService> {
        let log = Arc::new(Logger::new(paths.logfile(), false));
        // Surface an unopenable store here, where the caller can log it as the
        // reason the board is absent. The loop's own `SyncEngine` (holding
        // `!Send` boxed source clients) is constructed on its thread below.
        drop(comet_board::db::Db::open(&paths.db())?);
        let (tx, rx) = mpsc::channel::<Msg>();
        let (rows_tx, rows_rx) = watch::channel(Vec::<TaskRow>::new());
        let pin_tx = Arc::new(watch::Sender::new(OrchestratorPin::default()));
        let loop_pin = pin_tx.clone();

        // Forward every watch snapshot (current value first — the loop must
        // not treat "no snapshot yet" as "every chat is missing").
        let watch_tx = tx.clone();
        let watch_task = tokio::spawn(async move {
            loop {
                let snapshot = sessions.borrow_and_update().clone();
                if watch_tx.send(Msg::Sessions(snapshot)).is_err() {
                    return;
                }
                if sessions.changed().await.is_err() {
                    return;
                }
            }
        });

        let loop_paths = paths.clone();
        let thread = std::thread::Builder::new()
            .name("comet-board-sync".into())
            .spawn(
                move || match SyncEngine::from_paths(&loop_paths, log.clone()) {
                    Ok(engine) => {
                        let feeds = Feeds {
                            rows: rows_tx,
                            pin: loop_pin,
                        };
                        run_loop(engine, rx, log, runtime, handle, spaces, feeds)
                    }
                    Err(e) => log.error(format!("board loop failed to start: {e}")),
                },
            )?;
        tracing::info!("board service started");
        Ok(BoardService {
            tx,
            thread: std::sync::Mutex::new(Some(thread)),
            watch_task,
            rows: rows_rx,
            orchestrator: pin_tx,
            paths,
        })
    }

    /// This board's directories — where `ReadBoardConfig` / `WriteBoardConfig`
    /// find `routing.toml`.
    ///
    /// Available even when the sync loop failed to start: a config that does
    /// not parse stops the loop and is exactly the thing somebody then needs to
    /// read and fix.
    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// The board's rows, current value first — what `WatchBoard` streams.
    pub fn watch_rows(&self) -> watch::Receiver<Vec<TaskRow>> {
        self.rows.clone()
    }

    /// The pinned orchestrator, current value first — what
    /// `WatchBoardOrchestrator` streams (gh#104).
    pub fn watch_orchestrator(&self) -> watch::Receiver<OrchestratorPin> {
        self.orchestrator.subscribe()
    }

    /// A `routing.toml` write just landed: republish anything derived from it
    /// that a frontend is watching, without waiting for the loop's next reread.
    ///
    /// Only the pin today. Called with the same `RoutingView` the write
    /// returns — the parse of what is now on disk — so this cannot disagree
    /// with the reply the caller gets back. A file that does not parse leaves
    /// the pin as it was: the loop is still running on the last good config,
    /// and a frontend un-pinning itself over a syntax error would be a second
    /// lie on top of the first.
    pub fn note_config(&self, view: &comet_board::routes::RoutingView) {
        let Some(cfg) = view.config.as_ref() else {
            return;
        };
        publish_pin(&self.orchestrator, cfg.defaults.orchestrator());
    }

    /// Release a task: resolve its route, cut the checkout, create the chat,
    /// queue the brief. `origin` is who the caller says released it — the
    /// dispatching chat, device and human (gh#74) — provenance, never
    /// authority. `overrides` are the per-dispatch runtime/model/account
    /// choices that replace the route's defaults.
    pub async fn dispatch_task(
        &self,
        task_id: &str,
        origin: DispatchOrigin,
        overrides: DispatchOverrides,
    ) -> anyhow::Result<Dispatched> {
        self.dispatch_with(task_id, origin, overrides, false).await
    }

    /// Retry a task: end the live attempt the row is stuck on and start a new
    /// one — the blocked row's Retry (gh#49). [`BoardService::dispatch_task`]
    /// refuses a live attempt; this is the deliberate exception. The replaced
    /// attempt is interrupted and archived as `cancelled` first, then the
    /// release runs the normal dispatch flow.
    pub async fn retry_task(
        &self,
        task_id: &str,
        origin: DispatchOrigin,
        overrides: DispatchOverrides,
    ) -> anyhow::Result<Dispatched> {
        self.dispatch_with(task_id, origin, overrides, true).await
    }

    async fn dispatch_with(
        &self,
        task_id: &str,
        origin: DispatchOrigin,
        overrides: DispatchOverrides,
        replace: bool,
    ) -> anyhow::Result<Dispatched> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Dispatch {
                task_id: task_id.to_string(),
                origin,
                overrides,
                replace,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("board loop is not running"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("board loop went away mid-dispatch"))?
    }

    /// End a task's live attempt (interrupt + archive the chat). The issue
    /// stays open: cancel ends attempts, never tasks.
    pub async fn cancel_task(&self, task_id: &str) -> anyhow::Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Cancel {
                task_id: task_id.to_string(),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("board loop is not running"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("board loop went away mid-cancel"))?
    }

    /// One task's issue text, for a detail surface (gh#132).
    ///
    /// Errors when the board does not know the id, rather than answering with
    /// an empty body: "this issue has no description" and "there is no such row
    /// here" are different answers, and a reader that cannot tell them apart
    /// will show the wrong one.
    pub async fn task_detail(&self, task_id: &str) -> anyhow::Result<TaskDetail> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Detail {
                task_id: task_id.to_string(),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("board loop is not running"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("board loop went away mid-read"))?
    }

    /// What the board knows about its own throughput over a window (gh#143).
    /// `None` days is all time.
    pub async fn stats(&self, since_days: Option<i64>) -> anyhow::Result<BoardStats> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Stats { since_days, reply })
            .map_err(|_| anyhow::anyhow!("board loop is not running"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("board loop went away mid-read"))?
    }

    /// Stop the loop and wait for the in-flight cycle to finish, so shutdown
    /// never truncates a SQLite write mid-transaction.
    pub fn shutdown(&self) {
        self.watch_task.abort();
        let _ = self.tx.send(Msg::Shutdown);
        let handle = self
            .thread
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}

impl Drop for BoardService {
    fn drop(&mut self) {
        self.watch_task.abort();
        let _ = self.tx.send(Msg::Shutdown);
    }
}

/// What the loop publishes out to subscribers: the board's rows
/// (`WatchBoard`) and its pinned orchestrator (`WatchBoardOrchestrator`). One
/// struct because they are the loop's outputs and travel as a set.
struct Feeds {
    rows: watch::Sender<Vec<TaskRow>>,
    pin: Arc<watch::Sender<OrchestratorPin>>,
}

fn run_loop(
    mut engine: SyncEngine,
    rx: mpsc::Receiver<Msg>,
    log: Arc<Logger>,
    runtime: Arc<dyn Runtime + Send + Sync>,
    handle: tokio::runtime::Handle,
    spaces: watch::Receiver<Vec<Space>>,
    feeds: Feeds,
) {
    // The loop is a plain thread, but dispatch/cancel call engine code that
    // `tokio::spawn`s (doc_host.open) and `Handle::block_on`s (CometRuntime).
    // Enter the engine's runtime for the loop's life so those calls have a
    // context — a `tokio::spawn` outside any runtime panics and kills the
    // loop, which surfaces as "board stream interrupted" to the caller.
    let _enter = handle.enter();
    log.info(format!(
        "board loop up (interval {}s)",
        engine.cfg.sync.interval_secs()
    ));
    let mut statuses: Option<SessionStatuses> = None;
    // First cycle immediately: an operator starting the engine wants the board
    // fresh now, not in one interval.
    let mut next_sync = Instant::now();
    // Before the first cycle: a frontend that connects to a box whose board is
    // mid-poll should see the pin it already has, not "unpinned" for a whole
    // interval and then a glyph appearing under the cursor.
    publish_pin(&feeds.pin, engine.cfg.defaults.orchestrator());
    loop {
        if Instant::now() >= next_sync {
            // Credentials and routes are read at startup, but the engine
            // outlives the setup: adding a key or a repo takes effect on the
            // next cycle rather than requiring a restart.
            if let Some(fresh) = engine.reload_if_configuration_changed() {
                engine = fresh;
                // The reload is the only moment the pin can have moved: it is a
                // `routing.toml` key, so `WriteBoardConfig` from a laptop and an
                // `$EDITOR` over ssh both land here and nowhere else.
                publish_pin(&feeds.pin, engine.cfg.defaults.orchestrator());
            }
            match engine.sync_once_with(statuses.as_ref(), Some(runtime.as_ref())) {
                // Review delivery rides the cycle's own PR poll, and runs
                // after it so the `review` states it keys on are this tick's.
                Ok(pulls) => engine.deliver_reviews(runtime.as_ref(), &pulls),
                Err(e) => log.error(format!("sync cycle failed: {e}")),
            }
            publish_rows(&engine, &feeds.rows, &log);
            next_sync = Instant::now() + Duration::from_secs(engine.cfg.sync.interval_secs());
        }
        let wait = next_sync.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(Msg::Sessions(sessions)) => {
                let mapped = map_statuses(&sessions);
                // The event fast path: statuses, plus §H4's settle/reopen —
                // the clocked lifecycle (orphaning) stays on the interval.
                match engine.refresh_statuses_with(&mapped, Some(runtime.as_ref())) {
                    Ok(true) => publish_rows(&engine, &feeds.rows, &log),
                    Ok(false) => {}
                    Err(e) => log.warn(format!("refreshing agent statuses: {e}")),
                }
                statuses = Some(mapped);
            }
            Ok(Msg::Dispatch {
                task_id,
                origin,
                overrides,
                replace,
                reply,
            }) => {
                let result = handle_dispatch(
                    &engine,
                    runtime.as_ref(),
                    &spaces,
                    &task_id,
                    &origin,
                    &overrides,
                    replace,
                );
                match &result {
                    Ok(d) => log.info(format!(
                        "dispatched {task_id} → chat {} at {} (attempt {})",
                        d.chat_id, d.cwd, d.attempt
                    )),
                    Err(e) => log.error(format!("dispatch of {task_id} failed: {e:#}")),
                }
                publish_rows(&engine, &feeds.rows, &log);
                let _ = reply.send(result);
            }
            Ok(Msg::Cancel { task_id, reply }) => {
                let result = handle_cancel(&engine, runtime.as_ref(), &log, &task_id);
                if let Err(e) = &result {
                    log.error(format!("cancel of {task_id} failed: {e:#}"));
                }
                publish_rows(&engine, &feeds.rows, &log);
                let _ = reply.send(result);
            }
            // A read, so it publishes nothing and logs nothing: opening a row
            // is not an event on the board.
            // A read like `Detail`: publishes nothing, logs nothing. Opening
            // the stats page is not an event on the board.
            Ok(Msg::Stats { since_days, reply }) => {
                let result = engine
                    .db
                    .load_tasks()
                    .map(|tasks| comet_board::stats::gather(&tasks, since_days));
                let _ = reply.send(result);
            }
            Ok(Msg::Detail { task_id, reply }) => {
                let result = engine
                    .db
                    .get_task(&task_id)
                    .and_then(|task| {
                        task.ok_or_else(|| anyhow::anyhow!("{task_id} is not on the board"))
                    })
                    .map(|task| task_detail(&task));
                let _ = reply.send(result);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Ok(Msg::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                log.info("board loop stopping");
                return;
            }
        }
    }
}

/// Publish the pinned orchestrator to `WatchBoardOrchestrator` subscribers
/// (gh#104) — only when it actually changed, so the sidebars are not woken by
/// every cycle that read the same config back.
fn publish_pin(pin: &watch::Sender<OrchestratorPin>, chat_id: Option<&str>) {
    let fresh = OrchestratorPin {
        chat_id: chat_id.map(str::to_string),
    };
    pin.send_if_modified(|current| {
        if *current == fresh {
            return false;
        }
        *current = fresh;
        true
    });
}

/// Re-read the board and publish it to `WatchBoard` subscribers — only when it
/// actually changed, so a quiet 30s cycle does not wake every frontend.
fn publish_rows(engine: &SyncEngine, rows: &watch::Sender<Vec<TaskRow>>, log: &Logger) {
    match board_rows(&engine.db, &engine.cfg) {
        Ok(fresh) => {
            rows.send_if_modified(|current| {
                if *current == fresh {
                    return false;
                }
                *current = fresh;
                true
            });
        }
        Err(e) => log.error(format!("reading board rows: {e}")),
    }
}

/// One dispatch, on the loop thread (docs/BOARD.md §H2/H3).
///
/// The refusals come first — live attempt, route, capacity, space — so nothing
/// is created for a dispatch that cannot happen. Then ordering is deliberate:
/// the attempt row is inserted **first**, so the partial unique index refuses
/// a duplicate before anything is created. A failure after that closes the
/// attempt rather than leaving it live forever.
///
/// A retry (`replace`) is the one deliberate breach of the first refusal: it
/// ends the live attempt the row is stuck on before the release below, because
/// the one-live-attempt rule would otherwise refuse it — the whole point of a
/// blocked row's Retry is that the attempt exists. The cancellation runs
/// first, so the slot its chat held is free for the fresh attempt: ending a
/// stuck agent to replace it is what the operator asked for, and if the
/// release then fails the issue sits `ready` (the replaced attempt archived as
/// `cancelled`), never wedged.
fn handle_dispatch(
    engine: &SyncEngine,
    runtime: &(dyn Runtime + Send + Sync),
    spaces: &watch::Receiver<Vec<Space>>,
    task_id: &str,
    origin: &DispatchOrigin,
    overrides: &DispatchOverrides,
    replace: bool,
) -> anyhow::Result<Dispatched> {
    let task = engine
        .db
        .get_task(task_id)?
        .ok_or_else(|| anyhow::anyhow!("{task_id} is not on the board"))?;
    if !replace && let Some(live) = task.live_attempt() {
        anyhow::bail!(
            "{} already has a live attempt (chat {})",
            task.identifier,
            live.pane_id.as_deref().unwrap_or("pending")
        );
    }
    if replace && let Some(attempt) = task.live_attempt() {
        // The attempt the retry replaces may still hold a live chat; interrupt
        // it the way a cancel does, then archive the attempt so the fresh
        // release below can take its place. Its tokens are read first, for the
        // reason `handle_cancel` reads them first (gh#151).
        engine.record_tokens(Some(runtime), attempt);
        if let Some(chat_id) = attempt.pane_id.as_deref()
            && let Err(e) = runtime.cancel(chat_id)
        {
            engine.log.warn(format!("cancelling chat {chat_id}: {e:#}"));
        }
        engine.db.close_attempt(attempt.id, Outcome::Cancelled)?;
        engine.enqueue_outcome(&task, Outcome::Cancelled, None)?;
        engine.log.info(format!(
            "retrying {}: replaced live attempt {}",
            task.identifier, attempt.id
        ));
    }
    let route = route_for(&engine.cfg, &task)?;
    check_capacity(&engine.db, &engine.cfg, route)?;
    // Whose subscription this run spends, and the route's guard on it (gh#101).
    // Here, beside the cap, for the cap's reason: under `require-own` this is a
    // refusal, and a refusal that had already inserted an attempt row would
    // leave the operator cleaning up after a dispatch that never happened.
    let billing = comet_board::dispatch::check_billing(
        &engine.cfg,
        route,
        origin,
        overrides,
        |harness, account| {
            runtime.account_email(harness, account).unwrap_or_else(|e| {
                // A device that cannot read its own logins knows less, not
                // worse: the guard compares nothing and says nothing.
                engine
                    .log
                    .warn(format!("resolving the billed account: {e:#}"));
                None
            })
        },
    )?;
    // `warn` (and `require-own` on an acknowledged release) still owes both
    // parties a record. The log line is the box's copy; the attempt row, the
    // board row and the upstream comment below are everyone else's.
    if let Some(warning) = billing.as_ref().and_then(|b| b.warning())
        && engine.cfg.billing_guard(Some(route)).speaks()
    {
        engine.log.warn(format!("{}: {warning}", task.identifier));
    }
    let billed_to = billing.as_ref().and_then(|b| b.billed_to.clone());
    let cross_billed = billing.as_ref().is_some_and(|b| b.cross_billed());
    let space = spaces
        .borrow()
        .iter()
        .find(|s| space_matches(s.name.as_deref(), &s.path, &route.workspace))
        .map(|s| SpaceRef {
            id: s.id.clone(),
            device_id: s.device_id.clone(),
            path: s.path.clone(),
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no comet space named `{}` — the route exists, the space does not",
                route.workspace
            )
        })?;
    // Who released this, at the strongest form the transport allowed (gh#161):
    // the identity the edge verified for a relayed dispatch, the frontend's
    // claim for one issued on the box. Resolved once and used three times
    // below — the commit author, the attempt row and the upstream comment —
    // so those three cannot disagree about who this was.
    let by = origin.attribution();
    // `by` decides one thing in the spec: whose name the commits carry
    // (gh#107). Provenance, never authority — see `DispatchOrigin`.
    let spec = build_spec(&engine.cfg, route, &task, &space, overrides, by.name())?;
    // What the attempt actually runs under — the override, else the route's.
    let runtime_name = overrides.runtime.as_deref().unwrap_or(&route.runtime);

    // Provenance: a `via` chat with a live attempt names its task as the
    // parent; one without is still an agent if the chat is alive. The
    // `chat_alive` lookup runs only when it is the deciding fact.
    let dispatcher = dispatcher_for(&engine.db, origin.chat.as_deref(), |chat| {
        runtime.chat_alive(chat).unwrap_or(false)
    });

    let attempt_no = task.attempt_count() + 1;
    // The duplicate-dispatch guard: a second concurrent dispatch fails on the
    // partial unique index here, before a worktree or chat exists.
    let attempt_id = engine.db.insert_attempt(&NewAttempt {
        task_id: task.id.clone(),
        pane_id: None,
        workspace: route.workspace.clone(),
        runtime: runtime_name.to_string(),
        worktree: None,
        // Where the checkout will be cut from. Recorded now rather than beside
        // the worktree path afterwards, because it is the fact that outlives
        // the checkout: reclaiming the branch a week later runs in the repo
        // (gh#72).
        repo_path: Some(spec.repo_path.clone()),
        branch: Some(spec.branch.clone()),
        dispatched_by: dispatcher.task().map(str::to_string),
        dispatched_by_pane: dispatcher.pane().map(str::to_string),
        base_sha: None,
        // Whose subscription this attempt spends — the dispatch's choice, else
        // the route's. Recorded even though the chat row carries it too: the
        // route's default can change under a run that is still going, and the
        // attempt is the record of what actually ran (gh#59).
        account: spec.account.clone(),
        // Which device released it, and which human (gh#74) — with the
        // strength of that name beside it, because a verified dispatcher and a
        // frontend's claim are different records and only one of them is what
        // `require-own` refused on (gh#161).
        dispatched_by_device: origin.device.clone(),
        dispatched_by_user: by.name().map(str::to_string),
        dispatched_by_verified: by.is_verified(),
        // …and whose subscription it spends, resolved to an email now (gh#101).
        // Recorded whether or not it is cross-billed: the row says who is
        // paying for the attempt's whole life, and a field that only appeared
        // on the awkward dispatches would make its absence the interesting
        // signal.
        billed_to: billed_to.clone(),
    })?;

    match runtime.dispatch(&spec) {
        Ok(handle) => {
            engine.db.set_attempt_pane(attempt_id, &handle.chat_id)?;
            engine.db.set_attempt_worktree(attempt_id, &handle.cwd)?;
            // The checkout's own HEAD is the attempt's true starting point: for
            // a fresh branch it equals the repo HEAD, and for a reused one it
            // is the tip the agent builds on (herdr-board#10).
            match head_sha(&handle.cwd) {
                Some(sha) => engine.db.set_attempt_base_sha(attempt_id, &sha)?,
                None => engine.log.info(format!(
                    "{}: could not read HEAD of {} — completion falls back to a \
                     remote-relative commit count",
                    task.identifier, handle.cwd
                )),
            }
            // The upstream comment names the parent legibly: the issue
            // identifier when the board dispatched it too, the chat id
            // otherwise — not the raw task id. With no agent in the chain it
            // falls back to the human who released it (gh#74), which is the
            // difference between "dispatched by ana@example.com" and a comment
            // that says nothing about who released the work. Plain, without
            // gh#161's strength marker: this one is a public issue comment, and
            // the audience for "the box checked this" is the operator and the
            // orchestrator, not the repo.
            let by =
                dispatcher_name(&engine.db, &dispatcher).or_else(|| by.name().map(str::to_string));
            engine.enqueue_dispatch(
                &task,
                &route.runtime,
                &route.workspace,
                attempt_no,
                by.as_deref(),
                // Only when somebody else is paying, and only when the guard is
                // speaking at all: `off` is a board that has been told this is
                // nobody's business, and writing it onto a public issue anyway
                // would be the board overruling that.
                (cross_billed && engine.cfg.billing_guard(Some(route)).speaks())
                    .then_some(billed_to.as_deref())
                    .flatten(),
            )?;
            engine.rederive_all()?;
            Ok(Dispatched {
                chat_id: handle.chat_id,
                cwd: handle.cwd,
                attempt: attempt_no,
            })
        }
        Err(e) => {
            // Never leave a live attempt behind for a dispatch that did not
            // happen — it would block every retry via the unique index.
            engine.db.close_attempt(attempt_id, Outcome::Failed)?;
            Err(e)
        }
    }
}

/// One cancel, on the loop thread. The chat may already be gone; that is not a
/// reason to keep the attempt open. A row whose last attempt already closed
/// `failed`/`orphaned` can be cancelled too: that is the panel's reset — the
/// verdict flips to `cancelled` and the issue derives back to `ready` (gh#42).
fn handle_cancel(
    engine: &SyncEngine,
    runtime: &(dyn Runtime + Send + Sync),
    log: &Logger,
    task_id: &str,
) -> anyhow::Result<()> {
    let task = engine
        .db
        .get_task(task_id)?
        .ok_or_else(|| anyhow::anyhow!("{task_id} is not on the board"))?;
    if let Some(attempt) = task.live_attempt() {
        // Before the interrupt, while the chat is still there to ask: a cancel
        // never goes through reconcile, so this is the attempt's last chance
        // to record what it spent (gh#151).
        engine.record_tokens(Some(runtime), attempt);
        if let Some(chat_id) = attempt.pane_id.as_deref()
            && let Err(e) = runtime.cancel(chat_id)
        {
            log.warn(format!("cancelling chat {chat_id}: {e:#}"));
        }
        engine.db.close_attempt(attempt.id, Outcome::Cancelled)?;
        engine.enqueue_outcome(&task, Outcome::Cancelled, None)?;
        engine.rederive_all()?;
        log.info(format!("cancelled {}", task.identifier));
        return Ok(());
    }
    // No live attempt. A closed `failed`/`orphaned` attempt is the one case the
    // panel still offers cancel on: nothing to interrupt, just the outcome to
    // change. The same state matrix that derived `failed` from the outcome is
    // what `cancelled` un-derives to `ready`, so the operator can re-dispatch.
    if let Some(attempt) = task.last_closed_attempt()
        && matches!(attempt.outcome, Some(Outcome::Failed | Outcome::Orphaned))
    {
        engine
            .db
            .set_attempt_outcome(attempt.id, Outcome::Cancelled)?;
        engine.enqueue_outcome(&task, Outcome::Cancelled, None)?;
        engine.rederive_all()?;
        log.info(format!(
            "cancelled failed attempt on {} — back to ready",
            task.identifier
        ));
        return Ok(());
    }
    anyhow::bail!("{} has no live attempt", task.identifier);
}

fn head_sha(checkout: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", checkout, "rev-parse", "HEAD"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// This cycle's statuses, keyed by chat id, mapped through the same
/// `SessionStatus → AgentStatus` translation (with its staleness gate) that
/// `docs/BOARD.md` documents as the contract.
fn map_statuses(sessions: &[Session]) -> SessionStatuses {
    let now = chrono::Utc::now();
    sessions
        .iter()
        .map(|s| (s.chat_id.clone(), agent_status(Some(s), now)))
        .collect()
}

/// The `COMET_BOARD` kill switch — the board is on unless it says otherwise.
/// Cheap when unconfigured, and every frontend already knows how to render an
/// empty board.
pub fn board_enabled_from_env() -> bool {
    !matches!(
        std::env::var("COMET_BOARD").as_deref().map(str::trim),
        Ok("0") | Ok("false") | Ok("off")
    )
}

/// Statuses for one chat id from an ad-hoc session list — kept next to the
/// loop's mapping so the two cannot drift.
pub fn status_for(sessions: &[Session], chat_id: &str) -> AgentStatus {
    let now = chrono::Utc::now();
    agent_status(sessions.iter().find(|s| s.chat_id == chat_id), now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use comet_board::db::{Db, NewAttempt, UpsertTask};
    use comet_board::model::{BoardState, Source, UpstreamState};
    use comet_board::runtime::{DispatchHandle, DispatchSpec};
    use comet_proto::SessionStatus;

    fn scratch_paths() -> Paths {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "comet-board-service-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        let paths = Paths {
            config_dir: base.clone(),
            state_dir: base.join("state"),
        };
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        paths
    }

    /// Records what the board asked of comet; answers like a healthy engine.
    struct FakeRuntime {
        dispatched: std::sync::Mutex<Vec<DispatchSpec>>,
        cancelled: std::sync::Mutex<Vec<String>>,
        /// What `chat_alive` answers — flip to fake an archived/gone chat.
        alive: std::sync::atomic::AtomicBool,
        /// The device's saved logins, as `account_email` answers them (gh#101):
        /// slot id → email, with `""` standing for the box's own CLI login —
        /// the one a dispatch naming no slot actually reaches.
        logins: std::sync::Mutex<std::collections::HashMap<String, String>>,
        /// Mirror `CometRuntime::dispatch` → `doc_host.open`, which `tokio::spawn`s
        /// the chat task: when set, `dispatch` needs a tokio context. The board
        /// loop runs on a plain thread, so without the fix this panics and kills
        /// the loop — the regression the `dispatch_from_a_plain_thread...` test
        /// pins.
        spawn_in_dispatch: std::sync::atomic::AtomicBool,
    }

    impl Default for FakeRuntime {
        fn default() -> Self {
            Self {
                dispatched: Default::default(),
                cancelled: Default::default(),
                alive: std::sync::atomic::AtomicBool::new(true),
                logins: Default::default(),
                spawn_in_dispatch: std::sync::atomic::AtomicBool::new(false),
            }
        }
    }

    impl Runtime for FakeRuntime {
        fn dispatch(&self, spec: &DispatchSpec) -> anyhow::Result<DispatchHandle> {
            if self
                .spawn_in_dispatch
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                tokio::spawn(async move {});
            }
            self.dispatched.lock().unwrap().push(spec.clone());
            Ok(DispatchHandle {
                chat_id: format!("chat-for-{}", spec.identifier),
                cwd: format!("/worktrees/{}", spec.branch),
            })
        }
        fn prompt(&self, _chat_id: &str, _text: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn cancel(&self, chat_id: &str) -> anyhow::Result<()> {
            self.cancelled.lock().unwrap().push(chat_id.to_string());
            Ok(())
        }
        fn session(&self, _chat_id: &str) -> anyhow::Result<Option<Session>> {
            Ok(None)
        }
        fn chat_alive(&self, _chat_id: &str) -> anyhow::Result<bool> {
            Ok(self.alive.load(std::sync::atomic::Ordering::SeqCst))
        }
        fn chat_cwd(&self, _chat_id: &str) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
        fn account_email(
            &self,
            _harness: comet_proto::HarnessId,
            account: Option<&str>,
        ) -> anyhow::Result<Option<String>> {
            Ok(self
                .logins
                .lock()
                .unwrap()
                .get(account.unwrap_or(""))
                .cloned())
        }
        fn last_run_end(
            &self,
            _chat_id: &str,
        ) -> anyhow::Result<Option<comet_board::runtime::RunEnd>> {
            Ok(None)
        }
    }

    fn spawn_service(
        paths: &Paths,
        sessions: watch::Receiver<Vec<Session>>,
        spaces: Vec<Space>,
    ) -> (BoardService, Arc<FakeRuntime>) {
        let runtime = Arc::new(FakeRuntime::default());
        // A closed spaces channel still serves its last value via `borrow`.
        let (_, spaces_rx) = watch::channel(spaces);
        let service = BoardService::spawn_at(
            paths.clone(),
            sessions,
            runtime.clone(),
            spaces_rx,
            tokio::runtime::Handle::current(),
        )
        .unwrap();
        (service, runtime)
    }

    fn space(name: &str) -> Space {
        Space {
            id: format!("space-{name}"),
            device_id: "dev-1".into(),
            path: format!("/home/x/dev/{name}"),
            name: None,
            git_detected: true,
            git_checked_at: None,
            checkout_id: None,
            created_at: chrono::Utc::now(),
        }
    }

    fn seed_task(paths: &Paths, id: &str, identifier: &str) {
        let db = Db::open(&paths.db()).unwrap();
        db.upsert_task(&UpsertTask {
            id: id.into(),
            source: Source::Github,
            source_id: identifier.into(),
            identifier: identifier.into(),
            title: "Add retry".into(),
            body: None,
            url: "u".into(),
            labels: vec![],
            source_state: None,
            linear_team: None,
            linear_project: None,
            upstream: UpstreamState::Unstarted,
            updated_at: comet_board::db::now(),
        })
        .unwrap();
    }

    fn seed_attempt(paths: &Paths, task_id: &str, chat_id: &str) {
        seed_attempt_in(paths, task_id, chat_id, "offhand");
    }

    fn seed_attempt_in(paths: &Paths, task_id: &str, chat_id: &str, workspace: &str) {
        let db = Db::open(&paths.db()).unwrap();
        let a = db
            .insert_attempt(&NewAttempt {
                task_id: task_id.into(),
                pane_id: None,
                workspace: workspace.into(),
                runtime: "claude-code".into(),
                worktree: None,
                repo_path: None,
                branch: None,
                dispatched_by: None,
                dispatched_by_pane: None,
                base_sha: None,
                account: None,
                dispatched_by_device: None,
                dispatched_by_user: None,
                dispatched_by_verified: false,
                billed_to: None,
            })
            .unwrap();
        db.set_attempt_pane(a, chat_id).unwrap();
    }

    fn session(chat_id: &str, status: SessionStatus) -> Session {
        Session {
            chat_id: chat_id.into(),
            device_id: "dev-1".into(),
            status,
            started_at: None,
            updated_at: chrono::Utc::now(),
        }
    }

    fn write_route(paths: &Paths, workspace: &str, repo: &str) {
        std::fs::write(
            paths.routing(),
            format!(
                r#"
[[route]]
match = {{ gh_repo = "{repo}" }}
workspace = "{workspace}"
repo = "~/dev/{workspace}"
runtime = "mock"
"#
            ),
        )
        .unwrap();
    }

    /// Poll the board db until the derived state matches (the loop runs on its
    /// own thread) or a deadline passes.
    fn wait_for_state(paths: &Paths, task_id: &str, want: BoardState) -> BoardState {
        let db = Db::open(&paths.db()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let state = db
                .get_task(task_id)
                .unwrap()
                .map(|t| t.state)
                .unwrap_or(BoardState::Ready);
            if state == want || Instant::now() > deadline {
                return state;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_snapshot_lands_on_the_board() {
        let paths = scratch_paths();
        seed_task(&paths, "linear:LIN-142", "LIN-142");
        seed_attempt(&paths, "linear:LIN-142", "chat-1");

        let (tx, rx) = watch::channel(Vec::<Session>::new());
        let (service, _runtime) = spawn_service(&paths, rx, vec![]);

        // The agent asks a question → the board must show blocked without
        // waiting for an interval tick.
        tx.send(vec![session("chat-1", SessionStatus::AwaitingInput)])
            .unwrap();
        assert_eq!(
            wait_for_state(&paths, "linear:LIN-142", BoardState::Blocked),
            BoardState::Blocked
        );

        // ...and the answer puts it back to work the same way.
        tx.send(vec![session("chat-1", SessionStatus::Working)])
            .unwrap();
        assert_eq!(
            wait_for_state(&paths, "linear:LIN-142", BoardState::Working),
            BoardState::Working
        );

        service.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_ending_settles_an_attempt_with_a_pull_request() {
        // §H4 through the service: the session flipping Working → Idle is the
        // run-end event, and with a PR recorded the attempt closes on it —
        // no interval tick, no clock.
        let paths = scratch_paths();
        seed_task(&paths, "gh:owner/widget#4", "gh#4");
        seed_attempt(&paths, "gh:owner/widget#4", "chat-4");
        {
            let db = Db::open(&paths.db()).unwrap();
            db.set_pr(
                "gh:owner/widget#4",
                Some("https://github.com/owner/widget/pull/9"),
                Some(9),
                true,
            )
            .unwrap();
        }

        let (tx, rx) = watch::channel(Vec::<Session>::new());
        let (service, _runtime) = spawn_service(&paths, rx, vec![]);

        tx.send(vec![session("chat-4", SessionStatus::Working)])
            .unwrap();
        assert_eq!(
            wait_for_state(&paths, "gh:owner/widget#4", BoardState::Working),
            BoardState::Working
        );
        tx.send(vec![session("chat-4", SessionStatus::Idle)])
            .unwrap();
        assert_eq!(
            wait_for_state(&paths, "gh:owner/widget#4", BoardState::Review),
            BoardState::Review
        );
        let db = Db::open(&paths.db()).unwrap();
        let task = db.get_task("gh:owner/widget#4").unwrap().unwrap();
        assert!(task.live_attempt().is_none(), "the attempt settled");
        assert_eq!(
            task.attempts.last().and_then(|a| a.outcome),
            Some(Outcome::Done)
        );

        service.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dispatch_runs_the_runtime_and_records_the_attempt() {
        let paths = scratch_paths();
        write_route(&paths, "widget", "owner/widget");
        seed_task(&paths, "gh:owner/widget#7", "gh#7");

        let (_tx, rx) = watch::channel(Vec::<Session>::new());
        let (service, runtime) = spawn_service(&paths, rx, vec![space("widget")]);

        let d = service
            .dispatch_task(
                "gh:owner/widget#7",
                DispatchOrigin::via("chat-parent"),
                DispatchOverrides::default(),
            )
            .await
            .unwrap();
        assert_eq!(d.chat_id, "chat-for-gh#7");
        assert_eq!(d.attempt, 1);

        // The runtime got the resolved spec…
        let specs = runtime.dispatched.lock().unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].branch, "board/gh-7-widget");
        assert_eq!(specs[0].space_id, "space-widget");
        assert_eq!(specs[0].harness, comet_proto::HarnessId::Mock);
        drop(specs);

        // …and the attempt row carries the chat id + provenance.
        let db = Db::open(&paths.db()).unwrap();
        let task = db.get_task("gh:owner/widget#7").unwrap().unwrap();
        let attempt = task.live_attempt().expect("live attempt");
        assert_eq!(attempt.pane_id.as_deref(), Some("chat-for-gh#7"));
        assert_eq!(attempt.dispatched_by_pane.as_deref(), Some("chat-parent"));
        assert_eq!(attempt.branch.as_deref(), Some("board/gh-7-widget"));

        // A second dispatch is refused while the first attempt is live.
        let err = service
            .dispatch_task(
                "gh:owner/widget#7",
                DispatchOrigin::default(),
                DispatchOverrides::default(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("live attempt"), "{err}");

        // WatchBoard sees the dispatched row.
        let rows = service.watch_rows().borrow().clone();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].chat_id.as_deref(), Some("chat-for-gh#7"));

        service.shutdown();
    }

    /// Dispatch must survive the board loop's plain thread. `CometRuntime::dispatch`
    /// `tokio::spawn`s from it (doc_host.open's chat task), and a thread with no
    /// tokio context panics there — killing the loop, which surfaces as "board
    /// stream interrupted". A runtime whose dispatch really spawns reproduces it,
    /// and the whole call comes from a non-runtime thread.
    #[tokio::test(flavor = "multi_thread")]
    async fn dispatch_from_a_plain_thread_spawns_and_the_loop_survives() {
        let paths = scratch_paths();
        write_route(&paths, "widget", "owner/widget");
        seed_task(&paths, "gh:owner/widget#60", "gh#60");

        let (_tx, rx) = watch::channel(Vec::<Session>::new());
        let (service, runtime) = spawn_service(&paths, rx, vec![space("widget")]);
        let service = Arc::new(service);
        runtime
            .spawn_in_dispatch
            .store(true, std::sync::atomic::Ordering::SeqCst);

        // Dispatch through the board service from a plain thread: the loop
        // executes `handle_dispatch` → `dispatch` → `tokio::spawn` on
        // `comet-board-sync`, a `std::thread` that needs the engine's runtime
        // context to spawn.
        let handle = tokio::runtime::Handle::current();
        let dispatch = service.clone();
        let d = std::thread::spawn(move || {
            handle.block_on(async {
                dispatch
                    .dispatch_task(
                        "gh:owner/widget#60",
                        DispatchOrigin::default(),
                        DispatchOverrides::default(),
                    )
                    .await
                    .expect("dispatch succeeds")
            })
        })
        .join()
        .expect("dispatch thread did not panic");

        assert_eq!(d.chat_id, "chat-for-gh#60");
        assert_eq!(d.attempt, 1);

        // The loop survived: a second dispatch is refused on the live attempt
        // with a real answer, not "board loop is not running".
        let err = service
            .dispatch_task(
                "gh:owner/widget#60",
                DispatchOrigin::default(),
                DispatchOverrides::default(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("live attempt"), "{err}");

        // …and WatchBoard still streams the dispatched row.
        let rows = service.watch_rows().borrow().clone();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].chat_id.as_deref(), Some("chat-for-gh#60"));

        service.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_dispatch_runtime_override_reaches_the_spec_and_the_attempt_row() {
        let paths = scratch_paths();
        write_route(&paths, "widget", "owner/widget"); // route runtime = mock
        seed_task(&paths, "gh:owner/widget#50", "gh#50");

        let (_tx, rx) = watch::channel(Vec::<Session>::new());
        let (service, runtime) = spawn_service(&paths, rx, vec![space("widget")]);

        service
            .dispatch_task(
                "gh:owner/widget#50",
                DispatchOrigin::default(),
                DispatchOverrides {
                    runtime: Some("opencode".into()),
                    model: Some("gpt-5.2".into()),
                    account: None,
                    bill: None,
                },
            )
            .await
            .unwrap();

        // The runtime got the override, not the route's mock.
        let specs = runtime.dispatched.lock().unwrap();
        assert_eq!(specs[0].harness, comet_proto::HarnessId::Opencode);
        assert_eq!(specs[0].model.as_deref(), Some("gpt-5.2"));
        drop(specs);

        // …and the attempt row records what the agent actually runs under, so
        // the board's metadata and future retries name the real runtime.
        let db = Db::open(&paths.db()).unwrap();
        let task = db.get_task("gh:owner/widget#50").unwrap().unwrap();
        let attempt = task.live_attempt().expect("live attempt");
        assert_eq!(attempt.runtime, "opencode");
        let rows = service.watch_rows().borrow().clone();
        assert_eq!(rows[0].runtime.as_deref(), Some("opencode"));

        service.shutdown();
    }

    /// gh#74: a teammate's dispatch is recorded as theirs. The device and the
    /// human ride along as claims, land on the attempt row and the board row,
    /// and name the releaser in the upstream comment — where the row would
    /// otherwise say nothing about who released it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_dispatch_records_the_device_and_human_that_released_it() {
        let paths = scratch_paths();
        write_route(&paths, "widget", "owner/widget");
        seed_task(&paths, "gh:owner/widget#74", "gh#74");

        let (_tx, rx) = watch::channel(Vec::<Session>::new());
        let (service, _runtime) = spawn_service(&paths, rx, vec![space("widget")]);

        service
            .dispatch_task(
                "gh:owner/widget#74",
                DispatchOrigin {
                    chat: None,
                    device: Some("laptop-ana".into()),
                    user: Some("ana@example.com".into()),
                    verified: None,
                },
                DispatchOverrides::default(),
            )
            .await
            .unwrap();

        let db = Db::open(&paths.db()).unwrap();
        let task = db.get_task("gh:owner/widget#74").unwrap().unwrap();
        let attempt = task.live_attempt().expect("live attempt");
        assert_eq!(attempt.dispatched_by_device.as_deref(), Some("laptop-ana"));
        assert_eq!(
            attempt.dispatched_by_user.as_deref(),
            Some("ana@example.com")
        );
        // No agent in the chain: the operator is a person now, not an anonymous
        // `Dispatcher::Operator`.
        assert!(!attempt.dispatcher().is_agent());

        // The board row names the human; the device id stays off the wire.
        let rows = service.watch_rows().borrow().clone();
        assert_eq!(
            rows[0].dispatched_by_user.as_deref(),
            Some("ana@example.com")
        );

        // …and so does the comment queued for the issue.
        let queued = db.pending_writebacks(10).unwrap();
        let dispatch = queued
            .iter()
            .find(|w| w.kind == "dispatch")
            .expect("a dispatch writeback");
        assert!(
            dispatch.payload.contains("ana@example.com"),
            "the comment says who released it: {}",
            dispatch.payload
        );

        service.shutdown();
    }

    // ---- the billing guard (gh#101) --------------------------------------

    /// A route into `widget` with `billing_guard` as given, and no `account` —
    /// so a dispatch under it runs on the box's own login, which is the case
    /// the whole guard exists for.
    fn write_guarded_route(paths: &Paths, mode: &str) {
        std::fs::write(
            paths.routing(),
            format!(
                r#"
[[route]]
match = {{ gh_repo = "owner/widget" }}
workspace = "widget"
repo = "~/dev/widget"
runtime = "mock"

[defaults]
billing_guard = "{mode}"
"#
            ),
        )
        .unwrap();
    }

    /// The box's own login belongs to the owner; `ana` is the teammate at the
    /// keyboard, with a slot of her own.
    fn two_slots(runtime: &FakeRuntime) {
        let mut logins = runtime.logins.lock().unwrap();
        logins.insert(String::new(), "brede@tally.no".into());
        logins.insert("slot-box".into(), "brede@tally.no".into());
        logins.insert("slot-ana".into(), "ana@example.com".into());
    }

    /// gh#101's exit criterion on the box side: a teammate's enter-enter on a
    /// route with no account of its own runs on the owner's subscription, and
    /// the board says so — on the attempt, on the row, and in the comment that
    /// lands on the issue where both of them can see it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_cross_billed_dispatch_records_and_publishes_who_pays() {
        let paths = scratch_paths();
        write_guarded_route(&paths, "warn");
        seed_task(&paths, "gh:owner/widget#101", "gh#101");

        let (_tx, rx) = watch::channel(Vec::<Session>::new());
        let (service, runtime) = spawn_service(&paths, rx, vec![space("widget")]);
        two_slots(&runtime);

        service
            .dispatch_task(
                "gh:owner/widget#101",
                DispatchOrigin {
                    chat: None,
                    device: Some("laptop-ana".into()),
                    user: Some("ana@example.com".into()),
                    verified: None,
                },
                DispatchOverrides::default(),
            )
            .await
            .expect("warn releases");

        let db = Db::open(&paths.db()).unwrap();
        let task = db.get_task("gh:owner/widget#101").unwrap().unwrap();
        let attempt = task.live_attempt().expect("live attempt");
        assert_eq!(attempt.billed_to.as_deref(), Some("brede@tally.no"));
        // No slot was named, so the attempt's `account` is still None — which
        // is exactly why `billed_to` had to be resolved and stored: nothing
        // else on the row could name the payer.
        assert_eq!(attempt.account, None);

        // The board row carries it for the attempt's whole life…
        let rows = service.watch_rows().borrow().clone();
        assert_eq!(rows[0].billed_to.as_deref(), Some("brede@tally.no"));
        assert!(
            comet_proto::view::board::billing_note(&rows[0])
                .as_deref()
                .is_some_and(|note| note.contains("brede@tally.no")),
            "the shared derivation both viewports draw must say it"
        );

        // …and the upstream comment makes it public to both parties.
        let dispatch = db
            .pending_writebacks(10)
            .unwrap()
            .into_iter()
            .find(|w| w.kind == "dispatch")
            .expect("a dispatch writeback");
        assert!(
            dispatch.payload.contains("brede@tally.no"),
            "the comment names the payer: {}",
            dispatch.payload
        );

        service.shutdown();
    }

    /// Her own slot bills her, and nothing is said — the warning has to stay
    /// rare enough to mean something.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_dispatch_on_your_own_account_says_nothing_about_billing() {
        let paths = scratch_paths();
        write_guarded_route(&paths, "warn");
        seed_task(&paths, "gh:owner/widget#102", "gh#102");

        let (_tx, rx) = watch::channel(Vec::<Session>::new());
        let (service, runtime) = spawn_service(&paths, rx, vec![space("widget")]);
        two_slots(&runtime);

        service
            .dispatch_task(
                "gh:owner/widget#102",
                DispatchOrigin {
                    user: Some("ana@example.com".into()),
                    ..DispatchOrigin::default()
                },
                DispatchOverrides {
                    account: Some("slot-ana".into()),
                    ..DispatchOverrides::default()
                },
            )
            .await
            .unwrap();

        let db = Db::open(&paths.db()).unwrap();
        let task = db.get_task("gh:owner/widget#102").unwrap().unwrap();
        let attempt = task.live_attempt().expect("live attempt");
        // Recorded either way — the row says who pays for every attempt, not
        // only the awkward ones.
        assert_eq!(attempt.billed_to.as_deref(), Some("ana@example.com"));
        let rows = service.watch_rows().borrow().clone();
        assert_eq!(comet_proto::view::board::billing_note(&rows[0]), None);

        let dispatch = db
            .pending_writebacks(10)
            .unwrap()
            .into_iter()
            .find(|w| w.kind == "dispatch")
            .expect("a dispatch writeback");
        assert!(
            !dispatch.payload.contains("subscription"),
            "no suffix on a run that bills its own releaser: {}",
            dispatch.payload
        );

        service.shutdown();
    }

    /// `require-own` refuses beside the concurrency cap — before an attempt row
    /// exists, so the refusal costs no cleanup — and `--bill` naming the payer
    /// is what releases it.
    #[tokio::test(flavor = "multi_thread")]
    async fn require_own_refuses_a_cross_billed_release_until_it_names_the_payer() {
        let paths = scratch_paths();
        write_guarded_route(&paths, "require-own");
        seed_task(&paths, "gh:owner/widget#103", "gh#103");

        let (_tx, rx) = watch::channel(Vec::<Session>::new());
        let (service, runtime) = spawn_service(&paths, rx, vec![space("widget")]);
        two_slots(&runtime);

        let ana = DispatchOrigin {
            user: Some("ana@example.com".into()),
            ..DispatchOrigin::default()
        };
        let err = service
            .dispatch_task(
                "gh:owner/widget#103",
                ana.clone(),
                DispatchOverrides::default(),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("bills brede@tally.no's mock"), "{err}");
        assert!(err.contains("--bill brede@tally.no"), "{err}");

        // Nothing was created — no chat, no attempt row to clean up.
        assert!(runtime.dispatched.lock().unwrap().is_empty());
        let db = Db::open(&paths.db()).unwrap();
        assert!(
            db.get_task("gh:owner/widget#103")
                .unwrap()
                .unwrap()
                .live_attempt()
                .is_none()
        );
        drop(db);

        // Naming somebody else is a typo, not consent.
        let err = service
            .dispatch_task(
                "gh:owner/widget#103",
                ana.clone(),
                DispatchOverrides {
                    bill: Some("someone@else.example".into()),
                    ..DispatchOverrides::default()
                },
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("bills brede@tally.no"), "{err}");

        // Naming the payer releases it, and the record is unchanged by the
        // acknowledgement: the run still bills the owner and still says so.
        service
            .dispatch_task(
                "gh:owner/widget#103",
                ana,
                DispatchOverrides {
                    bill: Some("brede@tally.no".into()),
                    ..DispatchOverrides::default()
                },
            )
            .await
            .expect("named outright, it goes through");
        let db = Db::open(&paths.db()).unwrap();
        let task = db.get_task("gh:owner/widget#103").unwrap().unwrap();
        assert_eq!(
            task.live_attempt().unwrap().billed_to.as_deref(),
            Some("brede@tally.no")
        );

        service.shutdown();
    }

    /// `require-own` is about *whose*, not about *which*: the owner releasing
    /// their own work walks straight through, and so does a dispatch nobody
    /// attributed — an unattributed release names no wronged party.
    #[tokio::test(flavor = "multi_thread")]
    async fn require_own_only_refuses_a_run_charged_to_somebody_else() {
        let paths = scratch_paths();
        write_guarded_route(&paths, "require-own");
        seed_task(&paths, "gh:owner/widget#104", "gh#104");
        seed_task(&paths, "gh:owner/widget#105", "gh#105");

        let (_tx, rx) = watch::channel(Vec::<Session>::new());
        let (service, runtime) = spawn_service(&paths, rx, vec![space("widget")]);
        two_slots(&runtime);

        service
            .dispatch_task(
                "gh:owner/widget#104",
                DispatchOrigin {
                    user: Some("brede@tally.no".into()),
                    ..DispatchOrigin::default()
                },
                DispatchOverrides::default(),
            )
            .await
            .expect("the owner's own subscription");

        // No `viaUser` at all — `comet-board` from a shell nobody signed into,
        // or an orchestrating agent. Nothing to compare, so nothing refused.
        service
            .dispatch_task(
                "gh:owner/widget#105",
                DispatchOrigin::default(),
                DispatchOverrides::default(),
            )
            .await
            .expect("an unattributed dispatch is not an accusation");

        service.shutdown();
    }

    /// A box that cannot name the login accuses nobody — `off` likewise, and
    /// `off` additionally keeps the payer off the public issue, because a board
    /// told this is nobody's business must not publish it anyway.
    #[tokio::test(flavor = "multi_thread")]
    async fn off_releases_quietly_and_writes_nothing_upstream() {
        let paths = scratch_paths();
        write_guarded_route(&paths, "off");
        seed_task(&paths, "gh:owner/widget#106", "gh#106");

        let (_tx, rx) = watch::channel(Vec::<Session>::new());
        let (service, runtime) = spawn_service(&paths, rx, vec![space("widget")]);
        two_slots(&runtime);

        service
            .dispatch_task(
                "gh:owner/widget#106",
                DispatchOrigin {
                    user: Some("ana@example.com".into()),
                    ..DispatchOrigin::default()
                },
                DispatchOverrides::default(),
            )
            .await
            .unwrap();

        let db = Db::open(&paths.db()).unwrap();
        let dispatch = db
            .pending_writebacks(10)
            .unwrap()
            .into_iter()
            .find(|w| w.kind == "dispatch")
            .expect("a dispatch writeback");
        assert!(
            !dispatch.payload.contains("brede@tally.no"),
            "`off` is a board that has been told this is nobody's business: {}",
            dispatch.payload
        );
        // The attempt still records it — the mode governs what is *said*, not
        // what the board knows about its own runs.
        let task = db.get_task("gh:owner/widget#106").unwrap().unwrap();
        assert_eq!(
            task.live_attempt().unwrap().billed_to.as_deref(),
            Some("brede@tally.no")
        );

        service.shutdown();
    }

    /// A device that cannot say whose a login is leaves the guard with nothing
    /// to compare — and silence, not a refusal, is the only honest answer.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_box_that_cannot_name_its_logins_refuses_nothing() {
        let paths = scratch_paths();
        write_guarded_route(&paths, "require-own");
        seed_task(&paths, "gh:owner/widget#107", "gh#107");

        let (_tx, rx) = watch::channel(Vec::<Session>::new());
        // No logins registered: `account_email` answers `None` for everything.
        let (service, _runtime) = spawn_service(&paths, rx, vec![space("widget")]);

        service
            .dispatch_task(
                "gh:owner/widget#107",
                DispatchOrigin {
                    user: Some("ana@example.com".into()),
                    ..DispatchOrigin::default()
                },
                DispatchOverrides::default(),
            )
            .await
            .expect("unknown is not an accusation");

        let db = Db::open(&paths.db()).unwrap();
        let task = db.get_task("gh:owner/widget#107").unwrap().unwrap();
        assert_eq!(task.live_attempt().unwrap().billed_to, None);

        service.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bad_dispatch_runtime_override_is_refused_and_leaves_no_attempt() {
        let paths = scratch_paths();
        write_route(&paths, "widget", "owner/widget");
        seed_task(&paths, "gh:owner/widget#51", "gh#51");

        let (_tx, rx) = watch::channel(Vec::<Session>::new());
        let (service, runtime) = spawn_service(&paths, rx, vec![space("widget")]);

        let err = service
            .dispatch_task(
                "gh:owner/widget#51",
                DispatchOrigin::default(),
                DispatchOverrides {
                    runtime: Some("nonesuch".into()),
                    model: None,
                    account: None,
                    bill: None,
                },
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("nonesuch"), "{err}");

        // Refused before anything was created.
        assert!(runtime.dispatched.lock().unwrap().is_empty());
        let db = Db::open(&paths.db()).unwrap();
        let task = db.get_task("gh:owner/widget#51").unwrap().unwrap();
        assert!(task.live_attempt().is_none());

        service.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dispatch_is_refused_at_the_space_cap() {
        let paths = scratch_paths();
        std::fs::write(
            paths.routing(),
            r#"
[[route]]
match = { gh_repo = "owner/widget" }
workspace = "widget"
repo = "~/dev/widget"
runtime = "mock"

[defaults]
max_concurrent_per_workspace = 1
"#,
        )
        .unwrap();
        seed_task(&paths, "gh:owner/widget#20", "gh#20");
        seed_task(&paths, "gh:owner/widget#21", "gh#21");
        seed_attempt_in(&paths, "gh:owner/widget#20", "chat-20", "widget");

        let (_tx, rx) = watch::channel(Vec::<Session>::new());
        let (service, runtime) = spawn_service(&paths, rx, vec![space("widget")]);

        let err = service
            .dispatch_task(
                "gh:owner/widget#21",
                DispatchOrigin::default(),
                DispatchOverrides::default(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cancel one first"), "{err}");

        // Refused before anything was created: no chat, no attempt row.
        assert!(runtime.dispatched.lock().unwrap().is_empty());
        let db = Db::open(&paths.db()).unwrap();
        let task = db.get_task("gh:owner/widget#21").unwrap().unwrap();
        assert!(task.live_attempt().is_none());

        service.shutdown();
    }

    /// `via` naming a chat a live attempt owns resolves to that attempt's
    /// task: the chain reads `via gh#30`, not a bare chat id.
    #[tokio::test(flavor = "multi_thread")]
    async fn via_resolves_to_the_parent_task_when_the_board_dispatched_it() {
        let paths = scratch_paths();
        write_route(&paths, "widget", "owner/widget");
        seed_task(&paths, "gh:owner/widget#30", "gh#30");
        seed_task(&paths, "gh:owner/widget#31", "gh#31");
        // The parent works in another space, so the cap is not in play.
        seed_attempt_in(&paths, "gh:owner/widget#30", "chat-parent-30", "other");

        let (_tx, rx) = watch::channel(Vec::<Session>::new());
        let (service, _runtime) = spawn_service(&paths, rx, vec![space("widget")]);

        service
            .dispatch_task(
                "gh:owner/widget#31",
                DispatchOrigin::via("chat-parent-30"),
                DispatchOverrides::default(),
            )
            .await
            .unwrap();

        let db = Db::open(&paths.db()).unwrap();
        let task = db.get_task("gh:owner/widget#31").unwrap().unwrap();
        let attempt = task.live_attempt().expect("live attempt");
        assert_eq!(attempt.dispatched_by.as_deref(), Some("gh:owner/widget#30"));
        assert_eq!(
            attempt.dispatched_by_pane.as_deref(),
            Some("chat-parent-30")
        );

        service.shutdown();
    }

    /// A `via` chat that is archived or gone is not claimed as an agent; the
    /// dispatch is recorded as the operator's.
    #[tokio::test(flavor = "multi_thread")]
    async fn via_from_a_dead_chat_records_nobody() {
        let paths = scratch_paths();
        write_route(&paths, "widget", "owner/widget");
        seed_task(&paths, "gh:owner/widget#40", "gh#40");

        let (_tx, rx) = watch::channel(Vec::<Session>::new());
        let (service, runtime) = spawn_service(&paths, rx, vec![space("widget")]);
        runtime
            .alive
            .store(false, std::sync::atomic::Ordering::SeqCst);

        service
            .dispatch_task(
                "gh:owner/widget#40",
                DispatchOrigin::via("chat-gone"),
                DispatchOverrides::default(),
            )
            .await
            .unwrap();

        let db = Db::open(&paths.db()).unwrap();
        let task = db.get_task("gh:owner/widget#40").unwrap().unwrap();
        let attempt = task.live_attempt().expect("live attempt");
        assert!(attempt.dispatched_by.is_none());
        assert!(attempt.dispatched_by_pane.is_none());

        service.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dispatch_without_a_space_fails_and_leaves_no_live_attempt() {
        let paths = scratch_paths();
        write_route(&paths, "widget", "owner/widget");
        seed_task(&paths, "gh:owner/widget#8", "gh#8");

        let (_tx, rx) = watch::channel(Vec::<Session>::new());
        let (service, _runtime) = spawn_service(&paths, rx, vec![]);

        let err = service
            .dispatch_task(
                "gh:owner/widget#8",
                DispatchOrigin::default(),
                DispatchOverrides::default(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no comet space"), "{err}");

        let db = Db::open(&paths.db()).unwrap();
        let task = db.get_task("gh:owner/widget#8").unwrap().unwrap();
        assert!(task.live_attempt().is_none());

        service.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_interrupts_the_chat_and_closes_the_attempt() {
        let paths = scratch_paths();
        seed_task(&paths, "gh:owner/widget#9", "gh#9");
        seed_attempt(&paths, "gh:owner/widget#9", "chat-9");

        let (_tx, rx) = watch::channel(Vec::<Session>::new());
        let (service, runtime) = spawn_service(&paths, rx, vec![]);

        service.cancel_task("gh:owner/widget#9").await.unwrap();
        assert_eq!(runtime.cancelled.lock().unwrap().as_slice(), ["chat-9"]);

        let db = Db::open(&paths.db()).unwrap();
        let task = db.get_task("gh:owner/widget#9").unwrap().unwrap();
        assert!(task.live_attempt().is_none());
        assert_eq!(
            task.attempts.last().and_then(|a| a.outcome),
            Some(Outcome::Cancelled)
        );

        // Cancelling again is an error a caller can read, not a crash.
        let err = service.cancel_task("gh:owner/widget#9").await.unwrap_err();
        assert!(err.to_string().contains("no live attempt"), "{err}");

        service.shutdown();
    }

    /// The panel's cancel on a `failed` row has no live chat to interrupt — it
    /// flips the closed attempt's verdict so the issue re-derives to `ready`
    /// (gh#42), ready to be dispatched again.
    #[tokio::test(flavor = "multi_thread")]
    async fn canceling_a_failed_attempt_returns_the_issue_to_ready() {
        let paths = scratch_paths();
        seed_task(&paths, "gh:owner/widget#61", "gh#61");
        seed_attempt(&paths, "gh:owner/widget#61", "chat-61");
        let db = Db::open(&paths.db()).unwrap();
        let attempt_id = db
            .get_task("gh:owner/widget#61")
            .unwrap()
            .unwrap()
            .live_attempt()
            .unwrap()
            .id;
        db.close_attempt(attempt_id, Outcome::Orphaned).unwrap();
        drop(db);

        let (_tx, rx) = watch::channel(Vec::<Session>::new());
        let (service, runtime) = spawn_service(&paths, rx, vec![]);
        assert_eq!(
            wait_for_state(&paths, "gh:owner/widget#61", BoardState::Failed),
            BoardState::Failed
        );

        service.cancel_task("gh:owner/widget#61").await.unwrap();
        // Nothing live to interrupt — the dead attempt is just re-verdict'd.
        assert!(runtime.cancelled.lock().unwrap().is_empty());

        let db = Db::open(&paths.db()).unwrap();
        let task = db.get_task("gh:owner/widget#61").unwrap().unwrap();
        assert!(task.live_attempt().is_none());
        assert_eq!(
            task.attempts.last().and_then(|a| a.outcome),
            Some(Outcome::Cancelled)
        );
        assert_eq!(task.state, BoardState::Ready);

        service.shutdown();
    }

    /// Retry on a failed row is the same release flow as a `ready` one: the
    /// closed attempt is gone, so the unique index lets a fresh attempt in.
    #[tokio::test(flavor = "multi_thread")]
    async fn retry_after_a_failed_attempt_starts_a_new_one() {
        let paths = scratch_paths();
        write_route(&paths, "widget", "owner/widget");
        seed_task(&paths, "gh:owner/widget#62", "gh#62");
        seed_attempt_in(&paths, "gh:owner/widget#62", "chat-62", "widget");
        let db = Db::open(&paths.db()).unwrap();
        let attempt_id = db
            .get_task("gh:owner/widget#62")
            .unwrap()
            .unwrap()
            .live_attempt()
            .unwrap()
            .id;
        db.close_attempt(attempt_id, Outcome::Failed).unwrap();
        drop(db);

        let (_tx, rx) = watch::channel(Vec::<Session>::new());
        let (service, _runtime) = spawn_service(&paths, rx, vec![space("widget")]);

        let dispatched = service
            .dispatch_task(
                "gh:owner/widget#62",
                DispatchOrigin::default(),
                DispatchOverrides::default(),
            )
            .await
            .unwrap();
        assert_eq!(dispatched.attempt, 2);

        let db = Db::open(&paths.db()).unwrap();
        let task = db.get_task("gh:owner/widget#62").unwrap().unwrap();
        assert_eq!(task.attempt_count(), 2);
        assert!(task.live_attempt().is_some());
        assert_eq!(
            task.attempts[0].outcome,
            Some(Outcome::Failed),
            "the failed attempt stays on the record"
        );

        service.shutdown();
    }

    /// Retry on a blocked row is the one place the live-attempt rule is meant
    /// to be broken (gh#49): the row is stuck because its attempt IS alive and
    /// awaiting input, so retrying ends that attempt and releases a fresh one.
    #[tokio::test(flavor = "multi_thread")]
    async fn retry_on_a_live_attempt_replaces_it_with_a_fresh_one() {
        let paths = scratch_paths();
        write_route(&paths, "widget", "owner/widget");
        seed_task(&paths, "gh:owner/widget#63", "gh#63");
        seed_attempt_in(&paths, "gh:owner/widget#63", "chat-63", "widget");

        let (_tx, rx) = watch::channel(Vec::<Session>::new());
        let (service, runtime) = spawn_service(&paths, rx, vec![space("widget")]);

        // A plain dispatch is still refused while the attempt is live — only
        // the retry is allowed past the guard.
        let err = service
            .dispatch_task(
                "gh:owner/widget#63",
                DispatchOrigin::default(),
                DispatchOverrides::default(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("live attempt"), "{err}");

        // The retry interrupts the stuck chat and archives it…
        let dispatched = service
            .retry_task(
                "gh:owner/widget#63",
                DispatchOrigin::default(),
                DispatchOverrides::default(),
            )
            .await
            .unwrap();
        assert_eq!(dispatched.attempt, 2);
        assert_eq!(runtime.cancelled.lock().unwrap().as_slice(), ["chat-63"]);

        // …and a fresh attempt is live.
        let db = Db::open(&paths.db()).unwrap();
        let task = db.get_task("gh:owner/widget#63").unwrap().unwrap();
        assert_eq!(task.attempt_count(), 2);
        let live = task.live_attempt().expect("fresh attempt");
        assert_eq!(live.pane_id.as_deref(), Some("chat-for-gh#63"));
        assert_eq!(
            task.attempts[0].outcome,
            Some(Outcome::Cancelled),
            "the replaced attempt is archived as cancelled"
        );

        service.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_stops_the_loop() {
        let paths = scratch_paths();
        let (_tx, rx) = watch::channel(Vec::<Session>::new());
        let (service, _runtime) = spawn_service(&paths, rx, vec![]);
        // Returns only after the thread joins — a hang here is the failure.
        service.shutdown();
    }

    #[test]
    fn the_kill_switch_reads_only_explicit_offs() {
        // Not set in the test env → on. (Setting env vars in tests races other
        // tests, so only the default is pinned here.)
        assert!(board_enabled_from_env());
    }
}
