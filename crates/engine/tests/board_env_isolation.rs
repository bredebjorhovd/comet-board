//! gh#190: a board loop started by a test runs in the test's own directory.
//!
//! `syncd.log` on the box recorded 31 `board loop up` lines in one day, in
//! bursts, inside the windows in which dispatched agents were running
//! `cargo test`. The resolution that decides where that log goes decides where
//! `board.db` goes too, so those runs were reading the real queue on their way
//! past — the same failure gh#162 fixed in two tests by hand, in the places
//! nobody had looked yet.
//!
//! This binary runs with both of the board's directory variables set to a
//! directory nothing may touch — as every board-dispatched agent's environment
//! sets them — starts a real board service on a tempdir, and fails if a single
//! byte lands in the poisoned one. `crates/board/tests/env_isolation.rs` holds
//! the same guarantee one layer down, plus the scan that catches a new caller.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use comet_board::config::{CONFIG_DIR_ENV, Paths, STATE_DIR_ENV};
use comet_engine::{EngineCore, HarnessRegistry};
use comet_harness::mock::MockHarness;
use comet_proto::HarnessId;

/// Both variables, pointed somewhere real (so nothing fails merely for being
/// unwritable) that this test then requires to be untouched.
fn poison() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("comet-engine-poison-")
        .tempdir()
        .expect("poison dir");
    // SAFETY: this binary holds one test, and sets these before it starts any
    // thread of its own.
    unsafe {
        std::env::set_var(CONFIG_DIR_ENV, dir.path());
        std::env::set_var(STATE_DIR_ENV, dir.path());
    }
    dir
}

fn contents(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(contents(&path));
        } else {
            found.push(path);
        }
    }
    found
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_board_loop_under_a_dispatched_agents_environment_stays_in_its_tempdir() {
    let poisoned = poison();
    let dir = tempfile::tempdir().expect("tempdir");

    let data = dir.path().join("engine");
    std::fs::create_dir_all(&data).expect("data dir");
    std::fs::write(data.join("device-id"), "device-isolated").expect("device id");
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(MockHarness { script: Vec::new() }));
    let core = EngineCore::assemble(&data, Arc::new(registry), HarnessId::Mock, None)
        .expect("engine assembles");

    // The engine's own derivation, from the data dir it was configured with —
    // exactly what `BoardService::spawn` does in `Engine::assemble_runtime`.
    let paths = Paths::under(&data).expect("board dirs");
    let runtime = Arc::new(comet_engine::CometRuntime::new(
        core.repos.clone(),
        core.workspace.clone(),
        core.doc_host.clone(),
        core.workspace
            .merged_sessions_watch(core.sessions.watch_sessions()),
        core.sessions.journal(),
        core.agent_accounts.clone(),
        tokio::runtime::Handle::current(),
    ));
    let board = comet_engine::BoardService::spawn_at(
        paths.clone(),
        core.workspace
            .merged_sessions_watch(core.sessions.watch_sessions()),
        runtime,
        core.workspace.watch_spaces(),
        tokio::runtime::Handle::current(),
    )
    .expect("board service");
    core.set_board(Arc::new(board));

    // Wait for the loop to have written its own first line, so "nothing in the
    // poisoned directory" is a statement about a board that ran, not one that
    // had not started yet.
    let logged = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if std::fs::read_to_string(paths.logfile())
                .is_ok_and(|log| log.contains("board loop up"))
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await;
    assert!(
        logged.is_ok(),
        "the board loop never came up in {}",
        paths.logfile().display()
    );
    assert!(paths.db().exists(), "the store is the test's own");

    core.shutdown().await;

    let stray = contents(poisoned.path());
    assert!(
        stray.is_empty(),
        "a test board wrote into the directory the environment named — on the \
         box that is the live board's `syncd.log` and `board.db` (gh#190): {stray:?}"
    );
}
