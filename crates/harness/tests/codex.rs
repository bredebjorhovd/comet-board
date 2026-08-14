//! CodexHarness integration tests against the fake app server in
//! `tests/fixtures/fake-codex.sh` (no real `codex` binary involved).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use comet_harness::{
    CancellationToken, CodexHarness, Harness, HarnessError, RunControls, SteerMessage,
};
use comet_proto::{
    AgentEvent, DoneStatus, HarnessId, ReasoningLevel, RunRequest, SandboxLevel, TodoItem,
    ToolCall, UserInputAnswer, UserInputQuestion,
};

mod common;

/// Ceiling on any single fake-CLI run, before [`common::scaled`]. It covers a
/// child spawn and teardown as well as the scenario, so it scales with the
/// runner (gh#167).
const RUN_DEADLINE: Duration = Duration::from_secs(10);

fn fixture_path() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-codex.sh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    path
}

fn harness() -> CodexHarness {
    CodexHarness::new().with_executable(fixture_path())
}

fn request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        model: Some("gpt-5.6-sol".into()),
        reasoning: Some(ReasoningLevel::Ultra),
        model_options: serde_json::Map::new(),
        cwd: String::new(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        resume: None,
    }
}

/// Controls whose `request_input` answers every question with `answer_label`.
fn controls(
    answer_label: &'static str,
) -> (RunControls, mpsc::Sender<SteerMessage>, CancellationToken) {
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(move |questions| {
            let (tx, rx) = oneshot::channel();
            let answers: Vec<UserInputAnswer> = questions
                .iter()
                .map(|q| UserInputAnswer {
                    question_id: q.id.clone(),
                    labels: vec![answer_label.into()],
                })
                .collect();
            let _ = tx.send(answers);
            rx
        }),
        steering: steer_rx,
        interrupt: token.clone(),
        chat_id: None,
        account: None,
        push: None,
        bin_dirs: Vec::new(),
    };
    (controls, steer_tx, token)
}

async fn run_to_end(
    harness: &CodexHarness,
    req: RunRequest,
    controls: RunControls,
) -> Vec<AgentEvent> {
    let mut stream = harness.run(req, controls).await.expect("run starts");
    // Accumulated outside the timeout so a deadline failure can say what the
    // stream had emitted by then, not just that it did not end.
    let mut events = Vec::new();
    let drain = async {
        while let Some(ev) = stream.next().await {
            events.push(ev.expect("stream event"));
        }
    };
    let finished = tokio::time::timeout(common::scaled(RUN_DEADLINE), drain).await;
    assert!(
        finished.is_ok(),
        "run did not finish within {}\n  events before the deadline ({}):\n{}",
        common::deadline_note(RUN_DEADLINE),
        events.len(),
        common::events_so_far(&events),
    );
    events
}

#[tokio::test]
async fn happy_path_maps_deltas_items_usage_and_done() {
    let (controls, _steer, _token) = controls("Yes");
    let mut req = request("scenario:happy");
    req.cwd = "/tmp".into();
    req.model_options.insert(
        "serviceTier".into(),
        serde_json::Value::String("fast".into()),
    );
    let events = run_to_end(&harness(), req, controls).await;

    // SessionStarted from thread/start's thread id.
    let starts: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::SessionStarted {
                harness,
                model,
                cwd,
                session_id,
                ..
            } => Some((harness, model, cwd, session_id)),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 1, "{events:?}");
    let (h, model, cwd, session_id) = starts[0];
    assert_eq!(*h, HarnessId::Codex);
    assert_eq!(model, "gpt-5.6-sol");
    assert_eq!(cwd, "/tmp");
    assert_eq!(session_id, "th-1");

    // Deltas — both wire spellings accepted.
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "Hello".into()
    }));
    assert!(events.contains(&AgentEvent::ReasoningDelta {
        text: "thinking hard".into()
    }));
    assert!(events.contains(&AgentEvent::ReasoningDelta {
        text: "summary".into()
    }));

    // commandExecution: ToolCall at started only, exit code 1 => error result.
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolCall { id, .. } if id == "c1"))
            .count(),
        1
    );
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "c1".into(),
        call: ToolCall::Exec {
            command: "ls -la".into()
        },
    }));
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "c1".into(),
        is_error: true
    }));

    // fileChange (single add): WriteFile, refreshed at completion.
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(
                e,
                AgentEvent::ToolCall {
                    id,
                    call: ToolCall::WriteFile { path, content: None }
                } if id == "f1" && path == "/tmp/new.rs"
            ))
            .count(),
        2,
        "started + completion-refresh: {events:?}"
    );
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "f1".into(),
        is_error: false
    }));

    // mcpToolCall with failed status.
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "mcp1".into(),
        call: ToolCall::Mcp {
            server: "linear".into(),
            tool: "search".into(),
            input: Some(serde_json::json!({"q": "bug"})),
        },
    }));
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "mcp1".into(),
        is_error: true
    }));

    // webSearch lifecycle.
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "w1".into(),
        call: ToolCall::WebSearch {
            query: "rust".into()
        },
    }));
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "w1".into(),
        is_error: false
    }));

    // Completion-only todoList still opens and closes the lifecycle.
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "td1".into(),
        call: ToolCall::Todo {
            items: vec![
                TodoItem {
                    text: "a".into(),
                    done: true
                },
                TodoItem {
                    text: "b".into(),
                    done: false
                },
            ]
        },
    }));
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "td1".into(),
        is_error: false
    }));

    // Streamed agentMessage must not re-emit its completed text…
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta { text } if text == "Hello world")),
        "streamed message text re-emitted: {events:?}"
    );
    // …but a never-streamed one falls back to the completed text.
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "unstreamed tail".into()
    }));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::AssistantMessageCompleted { .. }))
            .count(),
        2
    );

    // Usage rides just before the terminal Done.
    let usage_pos = events
        .iter()
        .position(|e| {
            matches!(
                e,
                AgentEvent::Usage(comet_proto::TokenUsage {
                    input_tokens: 42,
                    output_tokens: 7,
                    ..
                })
            )
        })
        .expect("usage emitted");
    let done_pos = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Done { .. }))
        .expect("done emitted");
    assert!(usage_pos < done_pos);

    // Fullness is NOT held for the turn's end (gh#271): each snapshot that
    // moves the level is reported while the turn is still running, and the
    // straggler that arrives after `turn/completed` is dropped — a run journal
    // ending on anything but a `Done` reads as a run that died mid-stream.
    let levels: Vec<comet_proto::ContextUsage> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ContextUsage(usage) => Some(*usage),
            _ => None,
        })
        .collect();
    assert_eq!(levels.len(), 2, "one per move, none after Done: {levels:?}");
    assert_eq!(levels[0].used_tokens, 250_000);
    assert_eq!(levels[0].max_tokens, 272_000);
    assert_eq!(levels[0].percent(), Some(92));
    // Codex names no compaction point, so a ratio is all there is to judge on.
    assert!(levels[0].is_near_compaction(0.9));
    assert_eq!(levels[0].compact_at_tokens, None);
    // …and after the compaction the level is back down.
    assert_eq!(levels[1].percent(), Some(0));

    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("th-1".into()),
        })
    );
}

#[tokio::test]
async fn steering_uses_turn_steer_with_expected_turn_id() {
    let (controls, steer, _token) = controls("Yes");
    steer
        .send(SteerMessage {
            prompt: "redirect please".into(),
            message_id: None,
        })
        .await
        .expect("steer queued");
    let events = run_to_end(&harness(), request("scenario:steer"), controls).await;

    let steered = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Steered {
                assistant_message_id,
                next_assistant_message_id,
            } => Some((
                assistant_message_id.clone(),
                next_assistant_message_id.clone(),
            )),
            _ => None,
        })
        .expect("Steered emitted: {events:?}");
    assert!(steered.0.is_some() && steered.1.is_some());
    assert_ne!(steered.0, steered.1);

    // The fake only emits this delta after verifying expectedTurnId + text.
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "steered".into()
    }));
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("th-1".into()),
        })
    );
}

#[tokio::test]
async fn rejected_steer_falls_back_to_a_follow_up_turn() {
    let (controls, steer, _token) = controls("Yes");
    steer
        .send(SteerMessage {
            prompt: "redirect please".into(),
            message_id: None,
        })
        .await
        .expect("steer queued");
    let events = run_to_end(&harness(), request("scenario:steer-race"), controls).await;

    // Two turns: the raced one completes, then the fallback carries the steer.
    let dones: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Done { status, .. } => Some(*status),
            _ => None,
        })
        .collect();
    assert_eq!(
        dones,
        vec![DoneStatus::Completed, DoneStatus::Completed],
        "{events:?}"
    );
    let steered_pos = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Steered { .. }))
        .expect("Steered emitted on fallback");
    let first_done_pos = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Done { .. }))
        .expect("first done");
    assert!(
        first_done_pos < steered_pos,
        "fallback turn starts after the raced turn ends: {events:?}"
    );
    // Only emitted by the fake when the fallback turn/start carried the text.
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "fallback".into()
    }));
}

#[tokio::test]
async fn approvals_round_trip_as_input_requests() {
    // Approvals must reach the ENGINE's input bridge (`request_input`) — and
    // the harness must NOT emit its own `InputRequested`/`InputResolved`
    // twins: the bridge owns that lifecycle (it mints the request id the
    // resolver is parked under; a harness-emitted copy folded an unanswerable
    // duplicate chip into the doc).
    let asked: Arc<Mutex<Vec<UserInputQuestion>>> = Arc::new(Mutex::new(Vec::new()));
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let _steer = steer_tx;
    let token = CancellationToken::new();
    let seen = asked.clone();
    let controls = RunControls {
        request_input: Box::new(move |questions| {
            seen.lock().unwrap().extend(questions.iter().cloned());
            let (tx, rx) = oneshot::channel();
            let answers: Vec<UserInputAnswer> = questions
                .iter()
                .map(|q| UserInputAnswer {
                    question_id: q.id.clone(),
                    labels: vec!["Yes".into()],
                })
                .collect();
            let _ = tx.send(answers);
            rx
        }),
        steering: steer_rx,
        interrupt: token.clone(),
        chat_id: None,
        account: None,
        push: None,
        bin_dirs: Vec::new(),
    };
    let mut req = request("scenario:approve");
    req.auto_approve = false;
    let events = run_to_end(&harness(), req, controls).await;

    let asked = asked.lock().unwrap();
    assert_eq!(asked.len(), 2, "{events:?}");
    assert_eq!(asked[0].header, "Approve command");
    assert!(asked[0].question.contains("rm -rf /tmp/x"));
    assert_eq!(asked[0].options, vec!["Yes".to_string(), "No".to_string()]);
    assert_eq!(asked[1].header, "Approve file change");
    assert!(asked[1].question.contains("/tmp/a.rs"));
    assert!(
        !events.iter().any(|e| matches!(
            e,
            AgentEvent::InputRequested { .. } | AgentEvent::InputResolved { .. }
        )),
        "harness must not emit input lifecycle events itself: {events:?}"
    );

    // The fake only completes the turn after seeing BOTH accept decisions.
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("th-1".into()),
        })
    );
}

#[tokio::test]
async fn approval_no_answer_becomes_decline() {
    let (controls, _steer, _token) = controls("No");
    let mut req = request("scenario:decline");
    req.auto_approve = false;
    let events = run_to_end(&harness(), req, controls).await;

    // The fake only completes the turn after seeing the decline decision.
    assert!(
        matches!(
            events.last(),
            Some(AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            })
        ),
        "{events:?}"
    );
}

#[tokio::test]
async fn interrupt_sends_turn_interrupt_and_maps_aborted() {
    let (controls, _steer, token) = controls("Yes");
    let mut stream = harness()
        .run(request("scenario:interrupt"), controls)
        .await
        .expect("run starts");

    let mut events = Vec::new();
    let drain = async {
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(&ev, AgentEvent::TextDelta { text } if text == "working") {
                token.cancel(); // interrupt mid-turn
            }
            events.push(ev);
        }
    };
    let finished = tokio::time::timeout(common::scaled(RUN_DEADLINE), drain).await;
    assert!(
        finished.is_ok(),
        "the interrupted run did not end within {}\n  events before the deadline ({}):\n{}",
        common::deadline_note(RUN_DEADLINE),
        events.len(),
        common::events_so_far(&events),
    );

    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Interrupted,
            result: None,
            error: None,
            session_id: Some("th-1".into()),
        })
    );
}

#[tokio::test]
async fn unresponsive_child_is_reaped_with_interrupted_done() {
    let harness = CodexHarness::new()
        .with_executable(fixture_path())
        .with_graces(Duration::from_millis(100), Duration::from_millis(500));
    let (controls, _steer, token) = controls("Yes");
    let mut stream = harness
        .run(request("scenario:wedge"), controls)
        .await
        .expect("run starts");

    let mut events = Vec::new();
    let drain = async {
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(&ev, AgentEvent::TextDelta { text } if text == "working") {
                token.cancel();
            }
            events.push(ev);
        }
    };
    let finished = tokio::time::timeout(common::scaled(RUN_DEADLINE), drain).await;
    assert!(
        finished.is_ok(),
        "the wedged child was not escalated and reaped within {} — \
         the SIGKILL escalation never landed\n  events before the deadline ({}):\n{}",
        common::deadline_note(RUN_DEADLINE),
        events.len(),
        common::events_so_far(&events),
    );

    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Interrupted,
            result: None,
            error: None,
            session_id: Some("th-1".into()),
        })
    );
}

#[tokio::test]
async fn turn_failed_maps_to_errored_done() {
    let (controls, _steer, _token) = controls("Yes");
    let events = run_to_end(&harness(), request("scenario:fail"), controls).await;
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Errored,
            result: None,
            error: Some("boom".into()),
            session_id: Some("th-1".into()),
        })
    );
}

/// §gh#394. The sandbox failed to come up, so no command in the run can start
/// — not even `pwd`. The agent is alive and can still talk, which is exactly
/// the failure mode: it cannot push, cannot open a pull request, cannot claim,
/// and cannot signal blocked either, so the board row reads `working` until a
/// human notices the prose. The run has to end, and end as errored, which is
/// what the board reads as blocked and retryable.
#[tokio::test]
async fn a_sandbox_that_cannot_start_a_command_ends_the_run_instead_of_hanging() {
    let (controls, _steer, _token) = controls("Yes");
    let events = run_to_end(&harness(), request("scenario:sandbox-dead"), controls).await;

    let Some(AgentEvent::Done {
        status,
        error: Some(error),
        session_id,
        ..
    }) = events.last()
    else {
        panic!("the run must end with an errored Done: {events:?}");
    };
    assert_eq!(*status, DoneStatus::Errored);
    assert_eq!(session_id.as_deref(), Some("th-1"));
    assert!(
        error.contains("sandbox") && error.contains("Can't mkdir parents"),
        "the reason must name the sandbox and quote the line: {error}"
    );

    // The failed command is still on the record as a command — a reviewer sees
    // what was attempted, not only that the run died.
    assert!(
        events.contains(&AgentEvent::ToolResult {
            id: "c1".into(),
            is_error: true,
        }),
        "{events:?}"
    );
    // And the same reason is visible in the transcript, not only in the Done.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Error { message } if message.contains("sandbox"))),
        "{events:?}"
    );
}

#[tokio::test]
async fn resume_falls_back_to_fresh_thread() {
    let (controls, _steer, _token) = controls("Yes");
    let mut req = request("scenario:resumed");
    req.resume = Some("resume-fail".into());
    let events = run_to_end(&harness(), req, controls).await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::SessionStarted { session_id, .. } if session_id == "th-fresh"
        )),
        "fresh thread expected: {events:?}"
    );
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("th-fresh".into()),
        })
    );
}

#[tokio::test]
async fn resume_reuses_the_existing_thread() {
    let (controls, _steer, _token) = controls("Yes");
    let mut req = request("scenario:resumed");
    req.resume = Some("resume-ok".into());
    let events = run_to_end(&harness(), req, controls).await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::SessionStarted { session_id, .. } if session_id == "th-resumed"
        )),
        "resumed thread expected: {events:?}"
    );
}

#[tokio::test]
async fn missing_binary_is_not_installed() {
    let harness = CodexHarness::new().with_executable("/nonexistent/codex-nowhere");
    let (controls, _steer, _token) = controls("Yes");
    let err = harness
        .run(request("scenario:happy"), controls)
        .await
        .err()
        .expect("spawn fails");
    assert!(matches!(err, HarnessError::NotInstalled(_)), "{err:?}");
}

#[tokio::test]
async fn models_returns_curated_catalog() {
    let models = harness().models().await.expect("models");
    assert_eq!(models.len(), 7);
    assert_eq!(models[0].id, "gpt-5.6-sol");
    assert!(models[0].reasoning_levels.contains(&ReasoningLevel::Ultra));
    assert!(
        models
            .iter()
            .all(|m| m.options.iter().any(|o| o.id == "serviceTier"))
    );

    let missing = CodexHarness::new().with_executable("/nonexistent/codex-nowhere");
    // models() requires a resolvable binary… but with_executable trusts the
    // caller's path, so only the default resolution can report NotInstalled —
    // exercise the harness identity surface instead.
    assert_eq!(missing.id(), HarnessId::Codex);
    // "Codex" — comet composer/defaults.ts HARNESS_LABEL (and the registry's
    // lazy descriptor must stay in lockstep).
    assert_eq!(missing.display_name(), "Codex");
    assert_eq!(missing.reasoning_levels().len(), 7);
}

/// §gh#349. Codex's `workspace-write` denies writes to exactly `<cwd>/.git`, so
/// a dispatched agent in a main checkout can edit files and cannot commit them.
/// The fix is a permission grant, not a sandbox drop: the checkout's git
/// metadata rides `sandboxPolicy.writableRoots`, which the fixture asserts on
/// the wire — and the run announces the level it actually got before it says
/// anything else.
#[tokio::test]
async fn a_checkouts_git_dir_rides_the_wire_and_the_level_is_announced() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let checkout = tmp.path().join("repo");
    std::fs::create_dir_all(checkout.join(".git")).expect("fake checkout");

    let (controls, _steer, _token) = controls("Yes");
    let mut req = request("scenario:sandbox-roots");
    req.cwd = checkout.display().to_string();
    let events = run_to_end(&harness(), req, controls).await;

    // The fixture fails the turn when `writableRoots` is missing, so reaching
    // a completed Done IS the wire assertion.
    assert!(
        events.contains(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("th-1".into()),
        }),
        "the git dir did not reach sandboxPolicy.writableRoots: {events:?}"
    );

    // And the run said what it got, first, and before SessionStarted — a
    // journal's first line about a run is the terms it ran on.
    let reports: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Sandbox(report) => Some(report),
            _ => None,
        })
        .collect();
    assert_eq!(reports.len(), 1, "one per run: {events:?}");
    let report = reports[0];
    // A modern codex on a plain checkout keeps its sandbox: nothing widened,
    // and so nothing for a reviewer to be told.
    assert_eq!(report.requested, SandboxLevel::WorkspaceWrite);
    assert_eq!(report.effective, SandboxLevel::WorkspaceWrite);
    assert!(!report.widened_beyond_request());
    assert_eq!(report.note(), None);
    assert!(matches!(events[0], AgentEvent::Sandbox(_)), "{events:?}");
}
