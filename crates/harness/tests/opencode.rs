//! OpencodeHarness integration tests.
//!
//! - Against the fake serve in `src/bin/fake_opencode.rs` (no real `opencode`
//!   binary involved), driven by the prompt text like the codex/claude
//!   fixtures — these cover the gh#23 SSE-EOF/reaping behavior.
//! - Against the real `opencode` CLI, skipped (not failed) when the binary
//!   isn't on this device — the harness contract's end-to-end smoke.

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use comet_harness::{
    CancellationToken, Harness, HarnessError, OpencodeHarness, RunControls, SteerMessage,
};
use comet_proto::{
    AgentEvent, DoneStatus, HarnessId, ReasoningLevel, RunRequest, SandboxLevel, ToolCall,
};

mod common;

/// Ceiling on a fake-serve run, before [`common::scaled`]. Generous because it
/// covers a child spawn, an HTTP connect and a teardown on top of the ~2s the
/// scenarios actually need.
#[cfg(unix)]
const RUN_DEADLINE: Duration = Duration::from_secs(15);

/// Ceiling on the reap wait, before [`common::scaled`].
#[cfg(unix)]
const REAP_DEADLINE: Duration = Duration::from_secs(10);

/// Ceiling on a run against the real CLI, before [`common::scaled`] — a live
/// model turn, so an order of magnitude above the fake.
const REAL_RUN_DEADLINE: Duration = Duration::from_secs(120);

/// Where the fake serve records its pid, relative to the request's cwd.
#[cfg(unix)]
const PID_FILE: &str = "fake-opencode.pid";

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
        account: None,
        push: None,
        bin_dirs: Vec::new(),
        mcp_servers: Vec::new(),
    };
    (controls, steer_tx, token)
}

#[cfg(unix)]
async fn run_to_end(
    harness: &OpencodeHarness,
    req: RunRequest,
    controls: RunControls,
) -> Vec<AgentEvent> {
    let pid_file = PathBuf::from(&req.cwd).join(PID_FILE);
    let mut stream = harness.run(req, controls).await.expect("run starts");

    // Collected outside the timeout on purpose: what the stream managed to
    // emit before the deadline is the diagnosis, and `collect()` inside the
    // timeout throws it away.
    let mut events = Vec::new();
    let drain = async {
        while let Some(ev) = stream.next().await {
            events.push(ev.expect("stream event"));
        }
    };
    let finished = tokio::time::timeout(common::scaled(RUN_DEADLINE), drain).await;
    assert!(finished.is_ok(), "{}", timeout_report(&events, &pid_file));
    events
}

/// What the run had and had not done when the deadline fired.
///
/// The one bug class this suite exists to catch — a harness that hangs instead
/// of ending its stream — is indistinguishable from a slow runner unless the
/// failure separates the three cases: the child never started, the child ran
/// but the stream never ended, or the child is gone and the harness outlived
/// it. `expect("run finished in time")` said none of that, which is why gh#162
/// was merged past this rather than investigated.
#[cfg(unix)]
fn timeout_report(events: &[AgentEvent], pid_file: &Path) -> String {
    let serve = match std::fs::read_to_string(pid_file) {
        Err(err) => format!(
            "no pid file at {} ({err}) — the fake serve never got far enough to write one, \
             so this is a spawn/connect failure, not a hang in the harness",
            pid_file.display()
        ),
        Ok(raw) => match raw.trim().parse::<i32>() {
            Err(_) => format!("pid file holds {:?}, which is not a pid", raw.trim()),
            Ok(pid) if process_alive(pid) => format!(
                "fake serve pid {pid} is STILL ALIVE — the harness neither ended its stream \
                 nor reaped the child"
            ),
            Ok(pid) => format!(
                "fake serve pid {pid} is gone, but the stream never ended — the harness \
                 outlived the child it was reading from"
            ),
        },
    };
    format!(
        "run did not finish within {}\n  serve: {serve}\n  events before the deadline ({}):\n{}",
        common::deadline_note(RUN_DEADLINE),
        events.len(),
        common::events_so_far(events),
    )
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
    let pid: i32 = std::fs::read_to_string(dir.path().join(PID_FILE))
        .expect("fake serve wrote its pid")
        .trim()
        .parse()
        .expect("pid is numeric");
    let deadline = std::time::Instant::now() + common::scaled(REAP_DEADLINE);
    while std::time::Instant::now() < deadline {
        if !process_alive(pid) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "fake serve pid {pid} was still alive {} after the run ended — not reaped",
        common::deadline_note(REAP_DEADLINE)
    );
}

/// Regression for gh#310: every fake-serve test above depends on the fixture
/// playing its scenario, and the fixture used to be able to lose the cue.
///
/// The prompt and the `/event` subscription arrive on two different
/// connections, so nothing orders the prompt against the `/event` handler
/// reaching its wait. A dropped wakeup there is silent: the feed stays open and
/// empty, the harness has nothing to read, and the run hangs until the test's
/// deadline — which is what `idle_mid_turn_…` did on a loaded CI runner while
/// passing on a re-run of the same commit.
///
/// So this drives the fixture directly in the losing order — prompt first,
/// subscription second — which is the accident CI reproduced, made
/// deterministic. The cue has to be latched, not signalled.
#[cfg(unix)]
#[tokio::test]
async fn a_prompt_that_lands_before_the_feed_subscribes_still_plays() {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};

    let dir = tempfile::tempdir().expect("tempdir");
    let mut serve = tokio::process::Command::new(fake_bin())
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("fake serve spawns");
    let mut stdout = BufReader::new(serve.stdout.take().expect("stdout")).lines();
    let line = tokio::time::timeout(common::scaled(FEED_DEADLINE), stdout.next_line())
        .await
        .expect("fake serve announced its port in time")
        .expect("stdout readable")
        .expect("fake serve printed a listening line");
    let base = line
        .rsplit_once(' ')
        .map(|(_, url)| url.to_string())
        .expect("listening line carries the url");

    let http = reqwest::Client::new();
    // The prompt goes in before anything is listening to the feed.
    let prompted = http
        .post(format!("{base}/session/ses_fake/prompt_async"))
        .body(r#"{"parts":[{"type":"text","text":"scenario:stream-end"}]}"#)
        .send()
        .await
        .expect("prompt accepted");
    assert!(prompted.status().is_success(), "{:?}", prompted.status());

    let res = http
        .get(format!("{base}/event"))
        .send()
        .await
        .expect("feed subscribed");
    let mut feed = String::new();
    let played = tokio::time::timeout(common::scaled(FEED_DEADLINE), async {
        let mut chunks = res.bytes_stream();
        while let Some(chunk) = chunks.next().await {
            let Ok(bytes) = chunk else { break };
            feed.push_str(&String::from_utf8_lossy(&bytes));
            if feed.contains("message.part.delta") {
                return;
            }
        }
    })
    .await;
    assert!(
        played.is_ok(),
        "the feed never played the scenario within {} — the prompt that arrived \
         before this subscription was dropped. Feed so far: {feed:?}",
        common::deadline_note(FEED_DEADLINE),
    );
    let _ = serve.kill().await;
}

/// Ceiling on the fixture-level feed assertions, before [`common::scaled`].
#[cfg(unix)]
const FEED_DEADLINE: Duration = Duration::from_secs(10);

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

/// gh#233: what does the opencode child actually get?
///
/// The first opencode dispatch could not exec the board's askpass helper and
/// pushed with a wrapper of its own, which put the harness under suspicion —
/// maybe this adapter mangles the environment on the way down. It does not, and
/// this is where that stops being a belief. `opencode serve` is the parent of
/// every `git` an opencode agent runs, so the variables it received *are* the
/// ones those pushes inherit, and the fake writes them out.
///
/// What is asserted is the property that failed: `GIT_ASKPASS` arrives byte for
/// byte, and it names a file — because git execs it, and a value that is not a
/// path is a push that cannot authenticate.
#[cfg(unix)]
#[tokio::test]
async fn the_serve_child_gets_the_credential_environment_byte_for_byte() {
    let dir = tempfile::tempdir().expect("tempdir");
    // The shape a dispatched run is given: the board's shim, installed the way
    // the engine installs it, at a path with a space in it for good measure.
    let board = dir.path().join("comet board");
    std::fs::write(&board, "#!/bin/sh\necho x-access-token\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&board, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let askpass =
        comet_board::git_credentials::install_askpass_shim(&dir.path().join("bin"), &board)
            .expect("shim installed");
    let env = comet_board::git_credentials::push_env(&askpass, "owner/widget");

    let (mut controls, _steer, _token) = fake_controls();
    controls.push = Some(comet_harness::PushCredentials {
        env: env.clone(),
        bin_dir: None,
    });
    run_to_end(
        &harness(),
        fake_request("scenario:stream-end", &dir),
        controls,
    )
    .await;

    let seen: std::collections::BTreeMap<String, String> =
        std::fs::read_to_string(dir.path().join("fake-opencode.env"))
            .expect("the fake serve wrote its environment")
            .lines()
            .filter_map(|l| l.split_once('=').map(|(k, v)| (k.into(), v.into())))
            .collect();
    let handed: std::collections::BTreeMap<String, String> = env.into_iter().collect();
    for (key, value) in &handed {
        assert_eq!(
            seen.get(key),
            Some(value),
            "{key} did not survive the opencode child's launch: {seen:?}"
        );
    }
    let arrived = seen.get("GIT_ASKPASS").expect("GIT_ASKPASS reached serve");
    assert!(
        Path::new(arrived).is_file(),
        "GIT_ASKPASS names nothing git could exec: {arrived}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn spawned_opencode_receives_the_route_mcp_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut controls, _steer, _token) = fake_controls();
    controls.mcp_servers = vec![common::board_mcp_server()];

    run_to_end(
        &harness(),
        fake_request("scenario:stream-end", &dir),
        controls,
    )
    .await;

    let env = std::fs::read_to_string(dir.path().join("fake-opencode.env"))
        .expect("spawned OpenCode environment");
    let config = env
        .lines()
        .find_map(|line| line.strip_prefix("OPENCODE_CONFIG_CONTENT="))
        .expect("OpenCode inline config reached the server");
    let config: serde_json::Value = serde_json::from_str(config).unwrap();
    assert_eq!(
        config["mcp"]["comet-board"],
        serde_json::json!({
            "type": "local",
            "command": ["comet-board", "mcp"],
            "enabled": true
        })
    );
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

/// Regression for gh#79: a dying serve's session goes idle without settling its
/// assistant message, and the exit lands in the same instant — the stall path
/// (gh#37) and the child-exit path race, and the stall usually wins on a fast
/// host. The status is Errored either way, but the message is the diagnosis a
/// human reads: it must carry the real exit status, not "went idle" (which
/// sends them after a live-but-stuck server that isn't there).
#[cfg(unix)]
#[tokio::test]
async fn idle_racing_serve_exit_reports_the_exit_status_not_the_stall() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (controls, steer, _token) = fake_controls();
    drop(steer); // close the mailbox so the run settles after its turn
    let events = run_to_end(
        &harness(),
        fake_request("scenario:idle-then-crash", &dir),
        controls,
    )
    .await;

    let done = events.last().expect("terminal Done");
    match done {
        AgentEvent::Done {
            status,
            error: Some(error),
            ..
        } => {
            assert_eq!(*status, DoneStatus::Errored, "{events:?}");
            assert!(
                error.contains("exit code 1"),
                "the dead serve's exit status beats the stall wording: {error}"
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

/// Regression for gh#37: the session goes `Idle` mid-stream — text deltas and
/// real tool traffic were streamed but the assistant message never reached
/// `assistantMessageCompleted` (no `message.updated` with `time.completed`) and
/// no `session.error` fired (the exact fjaerpenn#1 journal shape). The terminal
/// event must be Errored — a stalled/aborted turn — never a spurious Completed.
#[cfg(unix)]
#[tokio::test]
async fn idle_mid_turn_without_message_completion_reports_errored() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (controls, steer, _token) = fake_controls();
    drop(steer); // close the mailbox so the run settles after its turn
    let events = run_to_end(
        &harness(),
        fake_request("scenario:idle-mid-turn", &dir),
        controls,
    )
    .await;

    assert!(
        events.contains(&AgentEvent::TextDelta {
            text: "hello".into()
        }),
        "turn streamed before the stall: {events:?}"
    );
    assert!(
        events.contains(&AgentEvent::ToolCall {
            id: "prt_tool1".into(),
            call: ToolCall::Exec {
                command: "ls".into()
            },
        }),
        "the stalled turn did real tool work before going idle: {events:?}"
    );
    assert!(
        events.contains(&AgentEvent::ToolResult {
            id: "prt_tool1".into(),
            is_error: false
        }),
        "the stalled turn's tool settled before the idle: {events:?}"
    );
    match events.last() {
        Some(AgentEvent::Done {
            status,
            error: Some(error),
            ..
        }) => {
            assert_eq!(*status, DoneStatus::Errored, "{events:?}");
            assert!(
                error.contains("assistant message"),
                "stall message should name the unsettled message: {error}"
            );
        }
        other => panic!("expected Errored Done for the mid-stream stall, got {other:?}"),
    }
    assert_serve_reaped(&dir).await;
}

/// Regression for gh#46: the harness pointed its `/event` SSE reader at a
/// reqwest client with a TOTAL request deadline, so a busy mid-run stream was
/// force-closed when the deadline fired (reqwest's `TotalTimeoutBody` returns
/// `TimedOut` even while events flow) — a genuinely-progressing run read as an
/// EOF stall and was killed Errored mid-answer. The stream must be exempt from
/// the total deadline (only the read timeout applies). Here the total deadline
/// is shrunk to 300ms while the fake server holds the feed open for 2s of
/// heartbeats before a settled turn — the run must still finish Completed.
#[cfg(unix)]
#[tokio::test]
async fn sse_stream_survives_past_the_total_request_deadline() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (controls, steer, _token) = fake_controls();
    drop(steer); // close the mailbox so the run settles after its turn
    // Deliberately NOT scaled: this 300ms is the thing under test, not a
    // margin. It has to expire while the fake still has 2s of heartbeats left
    // to send, so stretching it with the runner would quietly stop the test
    // from reproducing gh#46 at all.
    let harness = harness().with_request_timeout(Duration::from_millis(300));
    let events = run_to_end(&harness, fake_request("scenario:slow-turn", &dir), controls).await;

    assert!(
        events.contains(&AgentEvent::TextDelta {
            text: "hello".into()
        }),
        "turn streamed after the slow feed: {events:?}"
    );
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("ses_fake".into()),
        }),
        "the slow-but-progressing turn must complete, not stall: {events:?}"
    );
    assert_serve_reaped(&dir).await;
}

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
        account: None,
        push: None,
        bin_dirs: Vec::new(),
        mcp_servers: Vec::new(),
    };
    (controls, steer_tx, token)
}

#[tokio::test]
async fn real_opencode_streams_a_run_end_to_end() {
    let harness = OpencodeHarness::new();
    let (controls, steer, _token) = real_controls();
    // Close the steering mailbox so the persistent-session run settles after
    // its turn — the stream stays parked on an open mailbox (the old harness
    // only "ended" here because its 20s total request deadline closed the SSE
    // feed mid-run, gh#46).
    drop(steer);
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
        common::scaled(REAL_RUN_DEADLINE),
        stream.map(|r| r.expect("stream event")).collect(),
    )
    .await
    .unwrap_or_else(|_| {
        panic!(
            "real opencode run did not finish within {}",
            common::deadline_note(REAL_RUN_DEADLINE)
        )
    });

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
