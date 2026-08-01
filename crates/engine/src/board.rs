//! The board service — the engine hosting what herdr-board's `syncd` ran as a
//! separate daemon (docs/BOARD.md §H1).
//!
//! Two inputs drive the board:
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
use comet_board::log::Logger;
use comet_board::model::AgentStatus;
use comet_board::runtime::agent_status;
use comet_board::sync::{SessionStatuses, SyncEngine};
use comet_proto::Session;
use tokio::sync::watch;

enum Msg {
    Sessions(Vec<Session>),
    Shutdown,
}

/// Handle to the running board loop. Owned by the engine core; H2 grows the
/// RPC surface (`WatchBoard` / `DispatchTask` / `CancelTask`) on top of it.
pub struct BoardService {
    tx: mpsc::Sender<Msg>,
    thread: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    watch_task: tokio::task::JoinHandle<()>,
}

impl BoardService {
    /// Open the board under the engine's data dir and start the loop. Must be
    /// called from a tokio runtime (the session-watch forwarder is a task).
    pub fn spawn(
        data_dir: &std::path::Path,
        sessions: watch::Receiver<Vec<Session>>,
    ) -> anyhow::Result<BoardService> {
        Self::spawn_at(Paths::under(data_dir)?, sessions)
    }

    /// As [`BoardService::spawn`], with the directories chosen by the caller —
    /// the seam the tests use.
    pub fn spawn_at(
        paths: Paths,
        mut sessions: watch::Receiver<Vec<Session>>,
    ) -> anyhow::Result<BoardService> {
        let log = Arc::new(Logger::new(paths.logfile(), false));
        // Surface an unopenable store here, where the caller can log it as the
        // reason the board is absent. The loop's own `SyncEngine` (holding
        // `!Send` boxed source clients) is constructed on its thread below.
        drop(comet_board::db::Db::open(&paths.db())?);
        let (tx, rx) = mpsc::channel::<Msg>();

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
                Ok(engine) => run_loop(engine, rx, log),
                Err(e) => log.error(format!("board loop failed to start: {e}")),
            })?;
        tracing::info!("board service started");
        Ok(BoardService {
            tx,
            thread: std::sync::Mutex::new(Some(thread)),
            watch_task,
        })
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

fn run_loop(mut engine: SyncEngine, rx: mpsc::Receiver<Msg>, log: Arc<Logger>) {
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
            next_sync = Instant::now() + Duration::from_secs(engine.cfg.sync.interval_secs());
        }
        let wait = next_sync.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(Msg::Sessions(sessions)) => {
                let mapped = map_statuses(&sessions);
                // Status-only fast path; lifecycle stays on the interval clock.
                if let Err(e) = engine.refresh_statuses(&mapped) {
                    log.warn(format!("refreshing agent statuses: {e}"));
                }
                statuses = Some(mapped);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Ok(Msg::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                log.info("board loop stopping");
                return;
            }
        }
    }
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

/// Statuses for one chat id from an ad-hoc session list — H2's `Runtime`
/// implementation reads the same mirror; kept here so the mapping used by the
/// loop and by future callers cannot drift.
pub fn status_for(sessions: &[Session], chat_id: &str) -> AgentStatus {
    let now = chrono::Utc::now();
    agent_status(sessions.iter().find(|s| s.chat_id == chat_id), now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use comet_board::db::{Db, NewAttempt, UpsertTask};
    use comet_board::model::{BoardState, Source, UpstreamState};
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

    fn seed_attempt(paths: &Paths, chat_id: &str) {
        let db = Db::open(&paths.db()).unwrap();
        db.upsert_task(&UpsertTask {
            id: "linear:LIN-142".into(),
            source: Source::Linear,
            source_id: "uuid-1".into(),
            identifier: "LIN-142".into(),
            title: "Add retry".into(),
            body: None,
            url: "u".into(),
            labels: vec![],
            source_state: None,
            linear_team: Some("LIN".into()),
            linear_project: None,
            upstream: UpstreamState::Started,
            updated_at: comet_board::db::now(),
        })
        .unwrap();
        let a = db
            .insert_attempt(&NewAttempt {
                task_id: "linear:LIN-142".into(),
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

    /// Poll the board db until the derived state matches (the loop runs on its
    /// own thread) or a deadline passes.
    fn wait_for_state(paths: &Paths, want: BoardState) -> BoardState {
        let db = Db::open(&paths.db()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let state = db
                .get_task("linear:LIN-142")
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
        seed_attempt(&paths, "chat-1");

        let (tx, rx) = watch::channel(Vec::<Session>::new());
        let service = BoardService::spawn_at(paths.clone(), rx).unwrap();

        // The agent asks a question → the board must show blocked without
        // waiting for an interval tick.
        tx.send(vec![session("chat-1", SessionStatus::AwaitingInput)])
            .unwrap();
        assert_eq!(wait_for_state(&paths, BoardState::Blocked), BoardState::Blocked);

        // ...and the answer puts it back to work the same way.
        tx.send(vec![session("chat-1", SessionStatus::Working)])
            .unwrap();
        assert_eq!(wait_for_state(&paths, BoardState::Working), BoardState::Working);

        service.shutdown();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_stops_the_loop() {
        let paths = scratch_paths();
        let (_tx, rx) = watch::channel(Vec::<Session>::new());
        let service = BoardService::spawn_at(paths, rx).unwrap();
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
