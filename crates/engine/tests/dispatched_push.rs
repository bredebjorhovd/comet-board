//! gh#68: a dispatched agent's run carries the board's push credentials.
//!
//! The half that had no coverage before this change is the last hop — the one
//! that made #58's machinery a module with no callers. What is asserted here is
//! what the harness child would actually be spawned with: askpass wiring for
//! `git push`, a `gh` wrapper on PATH for `gh pr create`, the repo the attempt
//! belongs to, and no token anywhere in either.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use comet_board::config::Paths;
use comet_engine::push_credentials::PushCredentials;
use comet_engine::{EngineCore, HarnessRegistry};
use comet_harness::{Harness, HarnessError, RunControls};
use comet_proto::{
    AgentEvent, ChatConfig, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel,
    SteeringMode,
};

/// A harness that records the controls it was handed and ends the run. Keyed
/// by chat: the auto-titler drives runs of its own through the same registry,
/// and those are chat-less.
#[derive(Default)]
struct RecordingHarness {
    seen: Mutex<Vec<(Option<String>, Option<comet_harness::PushCredentials>)>>,
}

#[async_trait]
impl Harness for RecordingHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Recording"
    }
    fn supports_steering(&self) -> bool {
        false
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run(
        &self,
        _request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((controls.chat_id.clone(), controls.push.clone()));
        Ok(futures::stream::iter(vec![Ok(AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: None,
        })])
        .boxed())
    }
}

fn run_request(cwd: &str) -> RunRequest {
    RunRequest {
        prompt: "push it".into(),
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: cwd.into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        resume: None,
    }
}

fn chat_config(push_repo: Option<&str>) -> ChatConfig {
    ChatConfig {
        harness: HarnessId::Mock,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        sandbox: SandboxLevel::WorkspaceWrite,
        account: None,
        push_repo: push_repo.map(str::to_string),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dispatched_chats_run_carries_the_boards_credentials_and_a_plain_one_does_not() {
    let dir = std::env::temp_dir().join(format!("comet-gh68-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let data = dir.join("data");
    std::fs::create_dir_all(&data).unwrap();

    let harness = Arc::new(RecordingHarness::default());
    let registry = HarnessRegistry::new();
    registry.register(harness.clone());
    let core = EngineCore::assemble(&data, Arc::new(registry), HarnessId::Mock, None)
        .expect("engine core assembles");

    // A board with a credential, and a `comet-board` binary to run the helpers.
    let paths = Paths {
        config_dir: data.join("board"),
        state_dir: data.join("board").join("state"),
    };
    std::fs::create_dir_all(&paths.config_dir).unwrap();
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    std::fs::write(paths.config_dir.join(".env"), "GITHUB_TOKEN=ghp_secret\n").unwrap();
    let board_exe = dir.join("comet-board");
    std::fs::write(&board_exe, "").unwrap();
    core.sessions
        .set_push_credentials(Arc::new(PushCredentials::with_board_exe(
            paths.clone(),
            Some(board_exe.clone()),
        )));

    let device = core.device_id.clone();
    core.workspace
        .create_space("space-1", &device, "/tmp", Some("widget".into()), true)
        .unwrap();

    // The dispatched chat: the board stamped the repo its attempt works on.
    core.workspace
        .create_chat(
            "chat-dispatched",
            "space-1",
            Some(chat_config(Some("owner/widget"))),
            Some("/tmp".into()),
        )
        .unwrap();
    core.sessions
        .dispatch(
            "chat-dispatched",
            HarnessId::Mock,
            run_request("/tmp"),
            None,
        )
        .await
        .expect("run dispatches");

    // A chat somebody opened themselves: no repo, no credentials, and the
    // agent keeps pushing with whatever git the box has.
    core.workspace
        .create_chat(
            "chat-plain",
            "space-1",
            Some(chat_config(None)),
            Some("/tmp".into()),
        )
        .unwrap();
    core.sessions
        .dispatch("chat-plain", HarnessId::Mock, run_request("/tmp"), None)
        .await
        .expect("run dispatches");

    // `dispatch` returns once the run is spawned, not once the harness has it.
    let recorded = || {
        harness
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    };
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let chats: Vec<_> = recorded().into_iter().filter_map(|(id, _)| id).collect();
        if ["chat-dispatched", "chat-plain"]
            .iter()
            .all(|c| chats.iter().any(|id| id == c))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for both runs to reach the harness (saw {chats:?})"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let seen = recorded();
    let for_chat = |chat: &str| {
        seen.iter()
            .find(|(id, _)| id.as_deref() == Some(chat))
            .map(|(_, push)| push.clone())
            .unwrap_or_else(|| panic!("no run reached the harness for {chat}"))
    };

    let push = for_chat("chat-dispatched").expect("the dispatched run has credentials");
    let env: std::collections::BTreeMap<_, _> = push.env.iter().cloned().collect();
    assert_eq!(
        env.get("GIT_ASKPASS").map(String::as_str),
        Some(format!("'{}' git-askpass", board_exe.display()).as_str())
    );
    assert_eq!(
        env.get("COMET_BOARD_ASKPASS_REPO").map(String::as_str),
        Some("owner/widget")
    );
    // Terminal prompting off and the box's credential helper disabled: on a
    // headless box a prompt is a hang, and a keychain would cache an hourly
    // token forever.
    assert_eq!(
        env.get("GIT_TERMINAL_PROMPT").map(String::as_str),
        Some("0")
    );
    assert_eq!(env.get("GIT_CONFIG_VALUE_0").map(String::as_str), Some(""));
    // The helper is told which board to read, since it inherits none of the
    // engine's arguments.
    assert_eq!(
        env.get("COMET_BOARD_CONFIG_DIR").map(String::as_str),
        Some(paths.config_dir.display().to_string().as_str())
    );
    // Not one byte of the credential itself.
    assert!(
        !env.values().any(|v| v.contains("ghp_secret")),
        "the token reached the run's environment: {env:?}"
    );

    // `gh` gets a wrapper on PATH rather than a token in the environment —
    // when there is a `gh` on this box to wrap. Both are correct; what must
    // never happen is a GH_TOKEN sitting in the agent's own environment.
    if let Some(bin_dir) = &push.bin_dir {
        let script = std::fs::read_to_string(bin_dir.join("gh")).unwrap();
        assert!(script.contains("gh-token"), "{script}");
        assert!(!script.contains("ghp_secret"), "{script}");
    }
    assert!(
        !env.contains_key("GH_TOKEN"),
        "a token was exported into the agent's environment: {env:?}"
    );

    assert!(
        for_chat("chat-plain").is_none(),
        "a chat the board never dispatched was handed credentials"
    );

    core.sessions.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}
