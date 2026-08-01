//! The board service — the engine hosting what herdr-board's `syncd` ran as a
//! separate daemon (docs/BOARD.md §H1), plus the dispatch/cancel verbs and the
//! `WatchBoard` rows feed the RPC surface serves (§H2).
//!
//! Three inputs drive the board:
//!
//! - **The sync interval** (`[sync] interval` in `routing.toml`, default 30s):
//!   poll Linear/GitHub, reconcile session state, derive states, drain the
//!   writeback queue. This is the loop with lifecycle authority — orphaning
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
use comet_board::dispatch::{SpaceRef, build_spec, route_for, space_matches};
use comet_board::log::Logger;
use comet_board::model::{AgentStatus, Outcome};
use comet_board::rows::{TaskRow, board_rows};
use comet_board::runtime::{Runtime, agent_status};
use comet_board::sync::{SessionStatuses, SyncEngine};
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
        via: Option<String>,
        reply: oneshot::Sender<anyhow::Result<Dispatched>>,
    },
    Cancel {
        task_id: String,
        reply: oneshot::Sender<anyhow::Result<()>>,
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
}

impl BoardService {
    /// Open the board under the engine's data dir and start the loop. Must be
    /// called from a tokio runtime (the session-watch forwarder is a task).
    pub fn spawn(
        data_dir: &std::path::Path,
        sessions: watch::Receiver<Vec<Session>>,
        runtime: Arc<dyn Runtime + Send + Sync>,
        spaces: watch::Receiver<Vec<Space>>,
    ) -> anyhow::Result<BoardService> {
        Self::spawn_at(Paths::under(data_dir)?, sessions, runtime, spaces)
    }

    /// As [`BoardService::spawn`], with the directories chosen by the caller —
    /// the seam the tests use.
    pub fn spawn_at(
        paths: Paths,
        mut sessions: watch::Receiver<Vec<Session>>,
        runtime: Arc<dyn Runtime + Send + Sync>,
        spaces: watch::Receiver<Vec<Space>>,
    ) -> anyhow::Result<BoardService> {
        let log = Arc::new(Logger::new(paths.logfile(), false));
        // Surface an unopenable store here, where the caller can log it as the
        // reason the board is absent. The loop's own `SyncEngine` (holding
        // `!Send` boxed source clients) is constructed on its thread below.
        drop(comet_board::db::Db::open(&paths.db())?);
        let (tx, rx) = mpsc::channel::<Msg>();
        let (rows_tx, rows_rx) = watch::channel(Vec::<TaskRow>::new());

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

        let thread = std::thread::Builder::new()
            .name("comet-board-sync".into())
            .spawn(move || match SyncEngine::from_paths(&paths, log.clone()) {
                Ok(engine) => run_loop(engine, rx, log, runtime, spaces, rows_tx),
                Err(e) => log.error(format!("board loop failed to start: {e}")),
            })?;
        tracing::info!("board service started");
        Ok(BoardService {
            tx,
            thread: std::sync::Mutex::new(Some(thread)),
            watch_task,
            rows: rows_rx,
        })
    }

    /// The board's rows, current value first — what `WatchBoard` streams.
    pub fn watch_rows(&self) -> watch::Receiver<Vec<TaskRow>> {
        self.rows.clone()
    }

    /// Release a task: resolve its route, cut the checkout, create the chat,
    /// queue the brief. `via` is the dispatching chat's id when an agent
    /// released it — provenance, never authority.
    pub async fn dispatch_task(
        &self,
        task_id: &str,
        via: Option<String>,
    ) -> anyhow::Result<Dispatched> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Msg::Dispatch {
                task_id: task_id.to_string(),
                via,
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

fn run_loop(
    mut engine: SyncEngine,
    rx: mpsc::Receiver<Msg>,
    log: Arc<Logger>,
    runtime: Arc<dyn Runtime + Send + Sync>,
    spaces: watch::Receiver<Vec<Space>>,
    rows: watch::Sender<Vec<TaskRow>>,
) {
    log.info(format!(
        "board loop up (interval {}s)",
        engine.cfg.sync.interval_secs()
    ));
    let mut statuses: Option<SessionStatuses> = None;
    // First cycle immediately: an operator starting the engine wants the board
    // fresh now, not in one interval.
    let mut next_sync = Instant::now();
    loop {
        if Instant::now() >= next_sync {
            // Credentials and routes are read at startup, but the engine
            // outlives the setup: adding a key or a repo takes effect on the
            // next cycle rather than requiring a restart.
            if let Some(fresh) = engine.reload_if_configuration_changed() {
                engine = fresh;
            }
            if let Err(e) = engine.sync_once(statuses.as_ref()) {
                log.error(format!("sync cycle failed: {e}"));
            }
            publish_rows(&engine, &rows, &log);
            next_sync = Instant::now() + Duration::from_secs(engine.cfg.sync.interval_secs());
        }
        let wait = next_sync.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(Msg::Sessions(sessions)) => {
                let mapped = map_statuses(&sessions);
                // Status-only fast path; lifecycle stays on the interval clock.
                match engine.refresh_statuses(&mapped) {
                    Ok(true) => publish_rows(&engine, &rows, &log),
                    Ok(false) => {}
                    Err(e) => log.warn(format!("refreshing agent statuses: {e}")),
                }
                statuses = Some(mapped);
            }
            Ok(Msg::Dispatch {
                task_id,
                via,
                reply,
            }) => {
                let result = handle_dispatch(&engine, runtime.as_ref(), &spaces, &task_id, via);
                match &result {
                    Ok(d) => log.info(format!(
                        "dispatched {task_id} → chat {} at {} (attempt {})",
                        d.chat_id, d.cwd, d.attempt
                    )),
                    Err(e) => log.error(format!("dispatch of {task_id} failed: {e:#}")),
                }
                publish_rows(&engine, &rows, &log);
                let _ = reply.send(result);
            }
            Ok(Msg::Cancel { task_id, reply }) => {
                let result = handle_cancel(&engine, runtime.as_ref(), &log, &task_id);
                if let Err(e) = &result {
                    log.error(format!("cancel of {task_id} failed: {e:#}"));
                }
                publish_rows(&engine, &rows, &log);
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

/// One dispatch, on the loop thread (docs/BOARD.md §H2).
///
/// Ordering is deliberate: the attempt row is inserted **first**, so the
/// partial unique index refuses a duplicate before anything is created. A
/// failure after that closes the attempt rather than leaving it live forever.
///
/// H3 grows this: concurrency caps, resolving `via` into a parent task id, and
/// `COMET_BOARD_CHAT_ID` in the harness env.
fn handle_dispatch(
    engine: &SyncEngine,
    runtime: &(dyn Runtime + Send + Sync),
    spaces: &watch::Receiver<Vec<Space>>,
    task_id: &str,
    via: Option<String>,
) -> anyhow::Result<Dispatched> {
    let task = engine
        .db
        .get_task(task_id)?
        .ok_or_else(|| anyhow::anyhow!("{task_id} is not on the board"))?;
    if let Some(live) = task.live_attempt() {
        anyhow::bail!(
            "{} already has a live attempt (chat {})",
            task.identifier,
            live.pane_id.as_deref().unwrap_or("pending")
        );
    }
    let route = route_for(&engine.cfg, &task)?;
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
    let spec = build_spec(&engine.cfg, route, &task, &space)?;

    let attempt_no = task.attempt_count() + 1;
    // The duplicate-dispatch guard: a second concurrent dispatch fails on the
    // partial unique index here, before a worktree or chat exists.
    let attempt_id = engine.db.insert_attempt(&NewAttempt {
        task_id: task.id.clone(),
        pane_id: None,
        workspace: route.workspace.clone(),
        runtime: route.runtime.clone(),
        worktree: None,
        branch: Some(spec.branch.clone()),
        dispatched_by: None,
        dispatched_by_pane: via.clone(),
        base_sha: None,
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
            engine.enqueue_dispatch(
                &task,
                &route.runtime,
                &route.workspace,
                attempt_no,
                via.as_deref(),
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
/// reason to keep the attempt open.
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
    let Some(attempt) = task.live_attempt() else {
        anyhow::bail!("{} has no live attempt", task.identifier);
    };
    if let Some(chat_id) = attempt.pane_id.as_deref()
        && let Err(e) = runtime.cancel(chat_id)
    {
        log.warn(format!("cancelling chat {chat_id}: {e:#}"));
    }
    engine.db.close_attempt(attempt.id, Outcome::Cancelled)?;
    engine.enqueue_outcome(&task, Outcome::Cancelled, None)?;
    engine.rederive_all()?;
    log.info(format!("cancelled {}", task.identifier));
    Ok(())
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
    #[derive(Default)]
    struct FakeRuntime {
        dispatched: std::sync::Mutex<Vec<DispatchSpec>>,
        cancelled: std::sync::Mutex<Vec<String>>,
    }

    impl Runtime for FakeRuntime {
        fn dispatch(&self, spec: &DispatchSpec) -> anyhow::Result<DispatchHandle> {
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
            Ok(true)
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
        let service =
            BoardService::spawn_at(paths.clone(), sessions, runtime.clone(), spaces_rx).unwrap();
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
        let db = Db::open(&paths.db()).unwrap();
        let a = db
            .insert_attempt(&NewAttempt {
                task_id: task_id.into(),
                pane_id: None,
                workspace: "offhand".into(),
                runtime: "claude-code".into(),
                worktree: None,
                branch: None,
                dispatched_by: None,
                dispatched_by_pane: None,
                base_sha: None,
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
    async fn dispatch_runs_the_runtime_and_records_the_attempt() {
        let paths = scratch_paths();
        write_route(&paths, "widget", "owner/widget");
        seed_task(&paths, "gh:owner/widget#7", "gh#7");

        let (_tx, rx) = watch::channel(Vec::<Session>::new());
        let (service, runtime) = spawn_service(&paths, rx, vec![space("widget")]);

        let d = service
            .dispatch_task("gh:owner/widget#7", Some("chat-parent".into()))
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
            .dispatch_task("gh:owner/widget#7", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("live attempt"), "{err}");

        // WatchBoard sees the dispatched row.
        let rows = service.watch_rows().borrow().clone();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].chat_id.as_deref(), Some("chat-for-gh#7"));

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
            .dispatch_task("gh:owner/widget#8", None)
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
