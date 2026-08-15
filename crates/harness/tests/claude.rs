//! ClaudeHarness integration tests against the fake CLI in
//! `tests/fixtures/fake-claude.sh` (no real `claude` binary involved).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use comet_harness::{
    CancellationToken, ClaudeHarness, Harness, HarnessError, RunControls, SteerMessage,
};
use comet_proto::{
    AgentEvent, AgentKind, AgentTokenUsage, DoneStatus, HarnessId, ModelTokenUsage, RunRequest,
    SandboxLevel, TokenUsage, ToolCall, UserInputAnswer, UserInputQuestion,
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
        .join("fake-claude.sh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    path
}

fn harness() -> ClaudeHarness {
    ClaudeHarness::new().with_executable(fixture_path())
}

fn request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        model: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: String::new(),
        sandbox: SandboxLevel::DangerFullAccess,
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
        mcp_servers: Vec::new(),
    };
    (controls, steer_tx, token)
}

async fn run_to_end(
    harness: &ClaudeHarness,
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
async fn happy_path_normalizes_events_and_accounts_for_subagents() {
    let (controls, _steer, _token) = controls("A");
    let events = run_to_end(&harness(), request("scenario:happy"), controls).await;

    // One SessionStarted despite the re-emitted init frame.
    let starts: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::SessionStarted {
                harness,
                model,
                tools,
                session_id,
                ..
            } => Some((harness, model, tools, session_id)),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 1, "init must be deduped: {events:?}");
    let (h, model, tools, session_id) = starts[0];
    assert_eq!(*h, HarnessId::ClaudeCode);
    assert_eq!(model, "claude-fable-5");
    assert_eq!(tools, &vec!["Bash".to_string(), "Read".to_string()]);
    assert_eq!(session_id, "sess-1");

    assert!(events.contains(&AgentEvent::ReasoningDelta {
        text: "pondering".into()
    }));
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "Hello".into()
    }));

    // Subagent frames never become parent transcript content…
    assert!(
        !events.iter().any(|e| matches!(
            e,
            AgentEvent::TextDelta { text } if text.contains("SUBAGENT")
        )),
        "subagent delta leaked: {events:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCall { id, .. } | AgentEvent::ToolResult { id, .. } if id == "sub-tool"
        )),
        "subagent tool frames leaked: {events:?}"
    );
    // …but they are no longer silence (gh#280). The delegation is a named row,
    // the subagent's tool call is a counted step against it, and its token
    // stream beats as liveness the engine can see.
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "sub-1".into(),
        call: ToolCall::Task {
            description: "scan the tree".into(),
            subagent_type: Some("Explore".into()),
            steps: 0,
        },
    }));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(
                e,
                AgentEvent::SubagentActivity { parent_tool_use_id } if parent_tool_use_id == "sub-1"
            ))
            .count(),
        1,
        "one step per subagent tool call: {events:?}"
    );
    assert!(
        events.contains(&AgentEvent::ReasoningDelta {
            text: String::new()
        }),
        "subagent stream produced no liveness: {events:?}"
    );

    // Typed tool decoding: Bash -> Exec, mcp__server__tool -> Mcp.
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "tool-1".into(),
        call: ToolCall::Exec {
            command: "ls -la".into()
        },
    }));
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "tool-2".into(),
        call: ToolCall::Mcp {
            server: "linear".into(),
            tool: "search".into(),
            input: Some(serde_json::json!({"q": "bug"})),
        },
    }));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::AssistantMessageCompleted { .. }))
    );
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "tool-1".into(),
        is_error: false
    }));
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "tool-2".into(),
        is_error: true
    }));

    // Informational rate-limit frames stay quiet.
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));

    // The cache halves of the result frame reach the event too (gh#151) —
    // on a real session they are most of what the turn read.
    assert!(events.contains(&AgentEvent::Usage(comet_proto::TokenUsage {
        input_tokens: 10,
        output_tokens: 20,
        cache_creation_tokens: 300,
        cache_read_tokens: 4000,
    })));
    assert!(events.contains(&AgentEvent::AgentUsage(AgentTokenUsage {
        agent: AgentKind::Main,
        name: None,
        model: "claude-fable-5".into(),
        usage: TokenUsage {
            input_tokens: 4,
            output_tokens: 2,
            cache_creation_tokens: 100,
            cache_read_tokens: 1000,
        },
    })));
    assert!(events.contains(&AgentEvent::AgentUsage(AgentTokenUsage {
        agent: AgentKind::Subagent,
        name: Some("Explore".into()),
        model: "claude-spark-5".into(),
        usage: TokenUsage {
            input_tokens: 2,
            output_tokens: 4,
            cache_creation_tokens: 50,
            cache_read_tokens: 1500,
        },
    })));
    assert!(events.contains(&AgentEvent::ModelUsage {
        models: vec![
            ModelTokenUsage {
                model: "claude-fable-5".into(),
                usage: TokenUsage {
                    input_tokens: 8,
                    output_tokens: 16,
                    cache_creation_tokens: 250,
                    cache_read_tokens: 2500,
                },
            },
            ModelTokenUsage {
                model: "claude-spark-5".into(),
                usage: TokenUsage {
                    input_tokens: 2,
                    output_tokens: 4,
                    cache_creation_tokens: 50,
                    cache_read_tokens: 1500,
                },
            },
        ],
    }));
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: Some("done!".into()),
            error: None,
            session_id: Some("sess-1".into()),
        })
    );
}

#[cfg(unix)]
#[tokio::test]
async fn spawned_claude_receives_the_route_mcp_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (wrapper, argv_file) = common::recording_wrapper(&fixture_path(), &dir);
    let harness = ClaudeHarness::new().with_executable(wrapper);
    let (mut controls, _steer, _token) = controls("A");
    controls.mcp_servers = vec![common::board_mcp_server()];

    run_to_end(&harness, request("scenario:happy"), controls).await;

    let argv = std::fs::read_to_string(argv_file).expect("spawned Claude argv");
    let argv: Vec<&str> = argv.lines().collect();
    let config = argv
        .iter()
        .position(|arg| *arg == "--mcp-config")
        .and_then(|index| argv.get(index + 1))
        .expect("--mcp-config followed by its JSON value");
    let config: serde_json::Value = serde_json::from_str(config).unwrap();
    assert_eq!(
        config["mcpServers"]["comet-board"],
        serde_json::json!({"command": "comet-board", "args": ["mcp"]})
    );
}

/// Context fullness over the control channel (gh#271): polled while the turn
/// is live, reported when the level moves, and silent once the turn is over.
#[tokio::test]
async fn context_fullness_is_polled_while_the_turn_runs_and_never_after_it() {
    let (controls, _steer, _token) = controls("A");
    let harness = harness().with_context_poll(common::scaled(Duration::from_millis(20)));
    let events = run_to_end(&harness, request("scenario:context"), controls).await;

    let levels: Vec<comet_proto::ContextUsage> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ContextUsage(usage) => Some(*usage),
            _ => None,
        })
        .collect();
    assert_eq!(
        levels.len(),
        2,
        "two readings moved and the third repeated itself: {levels:?}"
    );
    assert_eq!(levels[0].used_tokens, 120_000);
    assert_eq!(levels[0].percent(), Some(60));
    assert_eq!(levels[1].percent(), Some(85));
    // Past the CLI's own auto-compact threshold, though short of any 90% rule
    // — the harness's number wins over a ratio we picked.
    assert!(levels[1].is_near_compaction(0.9));
    assert_eq!(levels[1].compact_at_tokens, Some(167_000));

    // The safety property: the fourth reply lands after the result frame, and
    // a journal whose last event is not a `Done` reads as a run that died.
    assert!(
        matches!(events.last(), Some(AgentEvent::Done { .. })),
        "a late reading must not follow the Done: {:?}",
        events.last()
    );
}

/// A CLI that will not answer the question costs the run nothing: no error, no
/// event, no stall. (An older CLI, or a `--remote` session with no callback.)
#[tokio::test]
async fn a_cli_that_cannot_report_context_usage_still_finishes_its_run() {
    let (controls, _steer, _token) = controls("A");
    let harness = harness().with_context_poll(common::scaled(Duration::from_millis(20)));
    let events = run_to_end(&harness, request("scenario:context-unsupported"), controls).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ContextUsage(_))),
        "an error reply is not a reading"
    );
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error { .. })));
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: Some("fine without it".into()),
            error: None,
            session_id: Some("sess-ctx-no".into()),
        })
    );
}

#[tokio::test]
async fn ask_user_question_round_trips_through_the_control_channel() {
    // The questions must reach the ENGINE's input bridge (`request_input`) —
    // and the harness must NOT emit its own `InputRequested`/`InputResolved`
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
                    labels: vec!["B".into()],
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
        mcp_servers: Vec::new(),
    };
    let events = run_to_end(&harness(), request("scenario:askuser"), controls).await;

    let asked = asked.lock().unwrap();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].header, "Choice");
    assert_eq!(asked[0].question, "Pick one");
    assert_eq!(asked[0].options, vec!["A".to_string(), "B".to_string()]);
    assert!(
        !events.iter().any(|e| matches!(
            e,
            AgentEvent::InputRequested { .. } | AgentEvent::InputResolved { .. }
        )),
        "harness must not emit input lifecycle events itself: {events:?}"
    );

    // "answered" proves both control round-trips: the plain Bash can_use_tool
    // was auto-allowed AND the answers reached the CLI as updatedInput.answers
    // keyed by question text.
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: Some("answered".into()),
            error: None,
            session_id: Some("sess-ask".into()),
        })
    );
}

#[tokio::test]
async fn steering_lines_are_written_to_stdin_mid_run() {
    let (controls, steer, _token) = controls("A");
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
        .expect("Steered emitted");
    assert!(steered.0.is_some() && steered.1.is_some());
    assert_ne!(steered.0, steered.1);

    // The fake CLI echoes the steer line's content back as a delta.
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "steered:redirect please".into()
    }));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        })
    ));
}

#[tokio::test]
async fn interrupt_escalates_to_sigterm_and_ends_with_interrupted_done() {
    let harness = ClaudeHarness::new()
        .with_executable(fixture_path())
        .with_graces(Duration::from_millis(100), Duration::from_millis(500));
    let (controls, _steer, token) = controls("A");
    let mut stream = harness
        .run(request("scenario:interrupt"), controls)
        .await
        .expect("run starts");

    let mut events = Vec::new();
    let drain = async {
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(ev, AgentEvent::SessionStarted { .. }) {
                token.cancel(); // interrupt as soon as the session is up
            }
            events.push(ev);
        }
    };
    let finished = tokio::time::timeout(common::scaled(RUN_DEADLINE), drain).await;
    assert!(
        finished.is_ok(),
        "the interrupted run did not end within {} — the SIGTERM escalation never \
         landed\n  events before the deadline ({}):\n{}",
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
            session_id: Some("sess-int".into()),
        })
    );
}

#[tokio::test]
async fn error_codes_map_to_readable_messages() {
    let (controls, _steer, _token) = controls("A");
    let events = run_to_end(&harness(), request("scenario:error"), controls).await;

    let errors: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Error { message } => Some(message.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        errors.contains(&"Claude usage limit reached — try again after the limit resets."),
        "assistant error code not mapped: {errors:?}"
    );
    assert!(
        errors.contains(
            &"Claude 5-hour limit reached — the turn was blocked. Try again after it resets."
        ),
        "rejected rate_limit_event not mapped: {errors:?}"
    );

    // Empty `errors` array on the result falls back to subtype wording.
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Errored,
            result: None,
            error: Some("The run hit the maximum number of turns.".into()),
            session_id: Some("sess-err".into()),
        })
    );
}

#[tokio::test]
async fn missing_binary_is_not_installed() {
    let harness = ClaudeHarness::new().with_executable("/nonexistent/claude-nowhere");
    let (controls, _steer, _token) = controls("A");
    let err = harness
        .run(request("scenario:happy"), controls)
        .await
        .err()
        .expect("spawn fails");
    assert!(matches!(err, HarnessError::NotInstalled(_)), "{err:?}");
}

/// Real-CLI proof of the context poll (gh#271): run a turn long enough to
/// outlast a poll interval and check the level that comes back is the CLI's
/// own — a window it named, a `/context` percentage, and the auto-compact
/// threshold to count down to. The fake CLI proves the plumbing; only this
/// proves the control-request subtype and the reply's field names are the ones
/// the installed binary actually speaks.
///
/// Ignored by default: needs an installed, authenticated `claude` CLI and
/// spends real tokens.
/// Run with: `cargo test -p comet-harness --test claude -- --ignored`
#[tokio::test]
#[ignore = "requires installed+authenticated claude CLI; spends tokens"]
async fn real_claude_answers_the_context_poll_mid_turn() {
    // The steering mailbox stays OPEN for the whole run, which is how the
    // engine runs it — and it has to: closing it shuts the child's stdin, and
    // stdin is the same channel the poll is written to. So the events are read
    // up to the turn's `Done` rather than to end of stream.
    let (controls, _steer, _token) = controls("A");
    // Fast enough that a short turn is still polled two or three times.
    let harness = ClaudeHarness::new().with_context_poll(Duration::from_secs(2));
    let mut request = request("Run `sleep 6` with the Bash tool, then reply DONE.");
    request.model = Some("haiku".into());
    request.cwd = std::env::temp_dir().display().to_string();

    let mut stream = match harness.run(request, controls).await {
        Ok(stream) => stream,
        Err(HarnessError::NotInstalled(_)) => {
            eprintln!("skipping: claude CLI not installed on this device");
            return;
        }
        Err(err) => panic!("claude run failed to start: {err}"),
    };
    let mut events: Vec<AgentEvent> = Vec::new();
    let drain = async {
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            let done = matches!(ev, AgentEvent::Done { .. });
            events.push(ev);
            if done {
                break;
            }
        }
    };
    tokio::time::timeout(common::scaled(Duration::from_secs(120)), drain)
        .await
        .expect("the turn finishes");

    // An unauthenticated box cannot answer anything; say so rather than fail
    // on a question that was never asked.
    if !events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            }
        )
    }) {
        eprintln!("skipping: the claude CLI could not take a turn: {events:?}");
        return;
    }

    let levels: Vec<comet_proto::ContextUsage> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ContextUsage(usage) => Some(*usage),
            _ => None,
        })
        .collect();
    let level = *levels.first().expect("the CLI answered a poll");
    assert!(level.max_tokens >= 100_000, "a real window: {level:?}");
    assert!(level.used_tokens > 0, "a loaded session is never empty");
    assert!(level.used_tokens <= level.max_tokens, "{level:?}");
    assert!(level.percent().is_some());
    // The CLI's own auto-compact point, which is the number the board counts
    // down to. (Absent only if the operator turned auto-compaction off.)
    assert!(
        level
            .compact_at_tokens
            .is_none_or(|at| at <= level.max_tokens),
        "{level:?}"
    );
    // …and never after the turn ended.
    assert!(matches!(events.last(), Some(AgentEvent::Done { .. })));
}
