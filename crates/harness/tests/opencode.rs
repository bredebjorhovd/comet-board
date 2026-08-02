//! OpencodeHarness integration tests.
//!
//! - Against the fake serve in `src/bin/fake_opencode.rs` (no real `opencode`
//!   binary involved), driven by the prompt text like the codex/claude
//!   fixtures — these cover the gh#23 SSE-EOF/reaping behavior.
//! - Against the real `opencode` CLI, skipped (not failed) when the binary
//!   isn't on this device — the harness contract's end-to-end smoke.

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use comet_harness::{
    CancellationToken, Harness, HarnessError, OpencodeHarness, RunControls, SteerMessage,
};
use comet_proto::{AgentEvent, DoneStatus, HarnessId, ReasoningLevel, RunRequest, SandboxLevel};

fn fake_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake_opencode"))
}

fn harness() -> OpencodeHarness {
    OpencodeHarness::new().with_executable(fake_bin())
}

// ---------------------------------------------------------------------------
// Fake-serve tests (unix: the reap assertions probe the child with kill(2)).
// ---------------------------------------------------------------------------

/// A request whose cwd is a fresh tempdir — the fake serve writes its pid
/// (`fake-opencode.pid`) there for the reap assertions.
#[cfg(unix)]
fn fake_request(prompt: &str, dir: &tempfile::TempDir) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        model: Some("deepseek/deepseek-v4-flash".into()),
        reasoning: Some(ReasoningLevel::Medium),
        model_options: serde_json::Map::new(),
        cwd: dir.path().to_str().unwrap().to_string(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        resume: None,
    }
}

#[cfg(unix)]
fn fake_controls() -> (RunControls, mpsc::Sender<SteerMessage>, CancellationToken) {
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(|_questions| {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(Vec::new());
            rx
        }),
        steering: steer_rx,
        interrupt: token.clone(),
        chat_id: None,
    };
    (controls, steer_tx, token)
}

#[cfg(unix)]
async fn run_to_end(
    harness: &OpencodeHarness,
    req: RunRequest,
    controls: RunControls,
) -> Vec<AgentEvent> {
    let stream = harness.run(req, controls).await.expect("run starts");
    tokio::time::timeout(
        Duration::from_secs(15),
        stream.map(|r| r.expect("stream event")).collect::<Vec<_>>(),
    )
    .await
    .expect("run finished in time")
}

/// True while the pid exists (a zombie counts — the harness must reap it).
#[cfg(unix)]
fn process_alive(pid: i32) -> bool {
    // SAFETY: kill(2) with signal 0 only probes existence; the pid is the fake
    // serve this test's harness spawned and (we assert) reaped.
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// The harness's run must have waited on its serve child: once the process is
/// reaped `kill(pid, 0)` reports ESRCH instead of the zombie still lingering
/// under us (the PPID-1-orphan failure mode this suite guards against).
#[cfg(unix)]
async fn assert_serve_reaped(dir: &tempfile::TempDir) {
    let pid: i32 = std::fs::read_to_string(dir.path().join("fake-opencode.pid"))
        .expect("fake serve wrote its pid")
        .trim()
        .parse()
        .expect("pid is numeric");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if !process_alive(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("fake serve pid {pid} was not reaped");
}

/// Regression for gh#23: opencode closes its SSE feed for reasons that are NOT
/// death (idle-parked / one-shot subscription). The harness must read the EOF
/// as a clean stream end — a Completed turn, never an Errored crash — and must
/// reap the serve process it spawned instead of leaving an orphan.
#[cfg(unix)]
#[tokio::test]
async fn stream_end_while_serve_alive_finishes_completed_and_reaps() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (controls, _steer, _token) = fake_controls();
    let events = run_to_end(
        &harness(),
        fake_request("scenario:stream-end", &dir),
        controls,
    )
    .await;

    assert!(
        events.contains(&AgentEvent::TextDelta {
            text: "hello".into()
        }),
        "turn streamed before the feed ended: {events:?}"
    );
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("ses_fake".into()),
        }),
        "{events:?}"
    );
    assert_serve_reaped(&dir).await;
}

/// A serve that actually dies mid-run must STILL surface as an Errored crash
/// (with the real exit status, not a "still running" shrug) and be reaped.
#[cfg(unix)]
#[tokio::test]
async fn serve_exit_mid_run_still_reports_errored_and_reaps() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (controls, _steer, _token) = fake_controls();
    let events = run_to_end(&harness(), fake_request("scenario:crash", &dir), controls).await;

    let done = events.last().expect("terminal Done: {events:?}");
    match done {
        AgentEvent::Done {
            status,
            error: Some(error),
            ..
        } => {
            assert_eq!(*status, DoneStatus::Errored, "{events:?}");
            assert!(
                error.contains("exit code 1"),
                "crash message should carry the real exit status: {error}"
            );
        }
        other => panic!("expected Errored Done with crash message, got {other:?}"),
    }
    assert_serve_reaped(&dir).await;
}

/// The normal persistent-session shape: the turn completes (Done on idle), the
/// run parks on the steering mailbox, and closing the mailbox ends the run —
/// which must reap the parked serve.
#[cfg(unix)]
#[tokio::test]
async fn happy_path_parks_then_reaps_when_steering_closes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (controls, steer, _token) = fake_controls();
    drop(steer); // close the mailbox so the run settles after its turn
    let events = run_to_end(&harness(), fake_request("scenario:happy", &dir), controls).await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::SessionStarted { harness, .. } if *harness == HarnessId::Opencode)),
        "{events:?}"
    );
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "hello".into()
    }));
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("ses_fake".into()),
        })
    );
    assert_serve_reaped(&dir).await;
}

// ---------------------------------------------------------------------------
// Real-CLI smoke tests (skipped when the binary isn't on this device).
// ---------------------------------------------------------------------------

fn real_request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        // Let opencode use its configured default model — the most portable
        // choice for a machine-specific integration test.
        model: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: std::env::temp_dir().display().to_string(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        resume: None,
        attachments: Vec::new(),
    }
}

fn real_controls() -> (RunControls, mpsc::Sender<SteerMessage>, CancellationToken) {
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(move |_questions| {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(Vec::new());
            rx
        }),
        steering: steer_rx,
        interrupt: token.clone(),
        chat_id: None,
    };
    (controls, steer_tx, token)
}

#[tokio::test]
async fn real_opencode_streams_a_run_end_to_end() {
    let harness = OpencodeHarness::new();
    let (controls, _steer, _token) = real_controls();
    let stream = match harness
        .run(real_request("Reply with exactly one word: ZORP."), controls)
        .await
    {
        Ok(stream) => stream,
        Err(HarnessError::NotInstalled(_)) => {
            eprintln!("skipping: opencode CLI not installed on this device");
            return;
        }
        Err(err) => panic!("opencode run failed to start: {err}"),
    };
    let events: Vec<AgentEvent> = tokio::time::timeout(
        Duration::from_secs(120),
        stream.map(|r| r.expect("stream event")).collect(),
    )
    .await
    .expect("run finished in time");

    let starts: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::SessionStarted {
                harness,
                session_id,
                ..
            } => Some((harness, session_id)),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 1, "{events:?}");
    assert_eq!(*starts[0].0, HarnessId::Opencode);
    assert!(starts[0].1.starts_with("ses_"), "session id: {starts:?}");

    // Streamed text, tool traffic, and a clean terminal Done.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta { text } if !text.is_empty())),
        "no text deltas: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            }
        )),
        "no completed Done: {events:?}"
    );
}

#[tokio::test]
async fn models_returns_a_catalog() {
    let harness = OpencodeHarness::new();
    let models = match harness.models().await {
        Ok(models) => models,
        Err(HarnessError::NotInstalled(_)) => {
            eprintln!("skipping: opencode CLI not installed on this device");
            return;
        }
        Err(err) => panic!("opencode models failed: {err}"),
    };
    assert!(!models.is_empty());
    for model in &models {
        assert!(
            model.id.contains('/'),
            "catalog ids are provider/model, got {}",
            model.id
        );
    }
    // Identity surface: the registry's lazy descriptor stays in lockstep.
    assert_eq!(harness.id(), HarnessId::Opencode);
    assert_eq!(harness.display_name(), "OpenCode");
}
