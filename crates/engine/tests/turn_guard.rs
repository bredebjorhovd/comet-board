//! gh#270 end-to-end: the run loop's turn guardrails.
//!
//! The decision is `comet_board::spin`'s and is unit-tested there on a list of
//! events. What this pins is the part only a live engine can show: a run whose
//! turn spins gets steered through its own mailbox, and — if that changes
//! nothing — is ended with an error the transcript keeps and the board reads as
//! a block. Plus the property that makes the feature safe to ship on by
//! default: a chat nobody dispatched is never touched.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use comet_engine::{EngineCore, HarnessRegistry, RunJournal};
use comet_harness::{Harness, HarnessError, RunControls};
use comet_proto::{
    AgentEvent, ChatConfig, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel,
    SessionStatus, SteeringMode, ToolCall, TurnLimits,
};

/// A harness stuck in the failure loop this feature exists for: it runs the
/// same command over and over, every one of them failing, and never ends its
/// own stream. Whatever it is told, it keeps going — which is the case the
/// hard rung has to cover.
#[derive(Default)]
struct SpinningHarness {
    /// Steers the engine pushed into the live run, in order.
    steers: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Harness for SpinningHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Spinning"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[ReasoningLevel::Medium]
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run(
        &self,
        _request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<AgentEvent, HarnessError>>(64);
        let token = controls.interrupt.clone();
        let mut steering = controls.steering;
        let seen = self.steers.clone();
        // The mailbox reader: a real harness delivers these into the child's
        // stdin. This one only records that the engine sent one — the point of
        // the test is that the *run loop* steers, and that a run which ignores
        // the advice is still stopped.
        tokio::spawn(async move {
            while let Some(message) = steering.recv().await {
                seen.lock().unwrap().push(message.prompt);
            }
        });
        tokio::spawn(async move {
            let started = AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "mock-1".into(),
                tools: vec![],
                cwd: "/tmp".into(),
                session_id: "hs-spin".into(),
                assistant_message_id: "a-1".into(),
            };
            if tx.send(Ok(started)).await.is_err() {
                return;
            }
            for i in 0.. {
                let id = format!("tool-{i}");
                let call = AgentEvent::ToolCall {
                    id: id.clone(),
                    call: ToolCall::Exec {
                        command: "cargo test -p widget".into(),
                    },
                };
                if tx.send(Ok(call)).await.is_err() {
                    return;
                }
                let result = AgentEvent::ToolResult { id, is_error: true };
                if tx.send(Ok(result)).await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            // Unreachable in practice; the engine's interrupt is what ends it.
            token.cancelled().await;
        });
        Ok(
            futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|e| (e, rx)) })
                .boxed(),
        )
    }
}

fn run_request() -> RunRequest {
    RunRequest {
        prompt: "fix the widget".into(),
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: "/tmp".into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        resume: None,
    }
}

fn chat_config(turn_limits: TurnLimits) -> ChatConfig {
    ChatConfig {
        harness: HarnessId::Mock,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        sandbox: SandboxLevel::WorkspaceWrite,
        account: None,
        push_repo: None,
        push_contract: None,
        git_author: None,
        turn_limits,
        mcp_servers: Vec::new(),
    }
}

/// Poll until `f` holds, or fail — the run loop settles asynchronously.
async fn until(label: &str, mut f: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {label}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_looping_dispatch_is_warned_once_and_then_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let harness = Arc::new(SpinningHarness::default());
    let steers = harness.steers.clone();
    let registry = HarnessRegistry::new();
    registry.register(harness);
    let core = EngineCore::assemble(dir.path(), Arc::new(registry), HarnessId::Mock, None)
        .expect("engine core assembles");

    let device = core.device_id.clone();
    core.workspace
        .create_space("space-1", &device, "/tmp", None, true)
        .unwrap();
    // A board dispatch: the route's guardrails are stamped on the chat.
    core.workspace
        .create_chat(
            "chat-dispatched",
            "space-1",
            Some(chat_config(TurnLimits {
                tool_failures: Some(3),
                tool_calls: None,
            })),
            Some("/tmp".into()),
        )
        .unwrap();

    core.sessions
        .dispatch("chat-dispatched", HarnessId::Mock, run_request(), None)
        .await
        .expect("run dispatches");

    // The soft rung: one message into the live run's mailbox, naming what is
    // looping — and exactly one, however long the stream goes on.
    until("the warning to reach the run's mailbox", || {
        !steers.lock().unwrap().is_empty()
    })
    .await;
    let warning = steers.lock().unwrap()[0].clone();
    assert!(
        warning.contains("`exec cargo test -p widget` has failed 3 times in a row"),
        "the warning names what is looping: {warning}"
    );

    // The hard rung: the run ends, errored — which is what the board reads off
    // the journal as a block, keeping the chat and its context for a retry.
    until("the run to be stopped", || {
        core.sessions
            .session_status("chat-dispatched")
            .map(|s| s.status)
            == Some(SessionStatus::Errored)
    })
    .await;
    assert_eq!(
        steers.lock().unwrap().len(),
        1,
        "warned once, not once per failing call"
    );

    // The journal's last word says why, in the terms the chat was told.
    let journal = RunJournal::open(dir.path().join("orgs/dev-org/dev-user/journals")).unwrap();
    let events = journal.replay("chat-dispatched", 0).unwrap();
    let (_, last) = events.last().expect("the run journaled something");
    match last {
        AgentEvent::Done { status, error, .. } => {
            assert_eq!(*status, DoneStatus::Errored);
            let error = error.clone().unwrap_or_default();
            assert!(error.contains("comet-board ended this run"), "{error}");
            assert!(error.contains("retry or cancel"), "{error}");
        }
        other => panic!("the run should end with a Done: {other:?}"),
    }
    // …and the transcript keeps a visible error part, not a bare aborted row.
    // Six, not three: the hard rung is twice the cap, counted from zero, so a
    // run whose warning changed nothing gets exactly as much rope again.
    assert!(
        events.iter().any(|(_, e)| matches!(
            e,
            AgentEvent::Error { message }
                if message.contains("`exec cargo test -p widget` has failed 6 times in a row")
        )),
        "the transcript says why it stopped"
    );
    // Nothing is left half-open for boot recovery to find.
    assert!(journal.stale_sessions().unwrap().is_empty());
}

/// The other half of the exit criterion: a chat a person opened carries no
/// limits, and the same runaway stream is left completely alone.
#[tokio::test(flavor = "multi_thread")]
async fn a_chat_nobody_dispatched_is_never_policed() {
    let dir = tempfile::tempdir().unwrap();
    let harness = Arc::new(SpinningHarness::default());
    let steers = harness.steers.clone();
    let registry = HarnessRegistry::new();
    registry.register(harness);
    let core = EngineCore::assemble(dir.path(), Arc::new(registry), HarnessId::Mock, None)
        .expect("engine core assembles");

    let device = core.device_id.clone();
    core.workspace
        .create_space("space-1", &device, "/tmp", None, true)
        .unwrap();
    core.workspace
        .create_chat(
            "chat-plain",
            "space-1",
            Some(chat_config(TurnLimits::default())),
            Some("/tmp".into()),
        )
        .unwrap();
    core.sessions
        .dispatch("chat-plain", HarnessId::Mock, run_request(), None)
        .await
        .expect("run dispatches");

    // Far more failures than any configured cap would have allowed.
    until("the run to get going", || {
        core.sessions.session_status("chat-plain").is_some()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(steers.lock().unwrap().is_empty(), "nobody was warned");
    assert_eq!(
        core.sessions.session_status("chat-plain").map(|s| s.status),
        Some(SessionStatus::Working),
        "the run is still going"
    );
    core.sessions.interrupt("chat-plain").await.unwrap();
}

/// A model change mid-session is a full-config replace, and the frontends build
/// that struct from their own picker state. It must not disarm guardrails or
/// erase the credential contract on a dispatched chat (gh#270, gh#440).
#[tokio::test(flavor = "multi_thread")]
async fn a_mid_session_config_change_keeps_board_owned_config() {
    let dir = tempfile::tempdir().unwrap();
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(SpinningHarness::default()));
    let core = EngineCore::assemble(dir.path(), Arc::new(registry), HarnessId::Mock, None)
        .expect("engine core assembles");
    let device = core.device_id.clone();
    core.workspace
        .create_space("space-1", &device, "/tmp", None, true)
        .unwrap();
    let limits = TurnLimits {
        tool_failures: Some(7),
        tool_calls: Some(700),
    };
    let push_contract = comet_proto::GithubPushContract {
        contents_write: true,
        workflows_write: true,
    };
    let mut dispatched_config = chat_config(limits);
    dispatched_config.push_repo = Some("owner/widget".into());
    dispatched_config.push_contract = Some(push_contract);
    core.workspace
        .create_chat(
            "chat-dispatched",
            "space-1",
            Some(dispatched_config),
            Some("/tmp".into()),
        )
        .unwrap();

    // What a composer sends when somebody picks a different model: everything
    // it knows about, which does not include the board's guardrails.
    let mut changed = chat_config(TurnLimits::default());
    changed.model = Some("mock-1".into());
    assert!(
        core.workspace
            .set_chat_config("chat-dispatched", &changed)
            .unwrap()
    );
    let stored = core.workspace.chat_config("chat-dispatched").unwrap();
    assert_eq!(stored.model.as_deref(), Some("mock-1"));
    assert_eq!(stored.turn_limits, limits);
    assert_eq!(stored.push_repo.as_deref(), Some("owner/widget"));
    assert_eq!(stored.push_contract, Some(push_contract));

    let mut retargeted = stored;
    retargeted.push_repo = Some("attacker/other".into());
    let error = core
        .workspace
        .set_chat_config("chat-dispatched", &retargeted)
        .unwrap_err()
        .to_string();
    assert!(error.contains("cannot change the board-owned"), "{error}");
}
