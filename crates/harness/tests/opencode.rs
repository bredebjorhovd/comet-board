//! OpencodeHarness integration tests against the real `opencode` CLI. Skipped
//! (not failed) when the binary isn't on this device — the harness contract is
//! still covered by the unit tests; this is the end-to-end smoke the harness
//! issue's definition of done calls for.

use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

use comet_harness::{
    CancellationToken, Harness, HarnessError, OpencodeHarness, RunControls, SteerMessage,
};
use comet_proto::{AgentEvent, DoneStatus, HarnessId, RunRequest, SandboxLevel, UserInputAnswer};

fn request(prompt: &str) -> RunRequest {
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

fn controls() -> (RunControls, mpsc::Sender<SteerMessage>, CancellationToken) {
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(move |_questions| {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(Vec::<UserInputAnswer>::new());
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
    let (controls, _steer, _token) = controls();
    let stream = match harness.run(request("Reply with exactly one word: ZORP."), controls).await {
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
                harness, session_id, ..
            } => Some((harness, session_id)),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 1, "{events:?}");
    assert_eq!(*starts[0].0, HarnessId::Opencode);
    assert!(starts[0].1.starts_with("ses_"), "session id: {starts:?}");

    // Streamed text, tool traffic, and a clean terminal Done.
    assert!(
        events.iter().any(|e| matches!(e, AgentEvent::TextDelta { text } if !text.is_empty())),
        "no text deltas: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(e, AgentEvent::Done { status: DoneStatus::Completed, .. })),
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
