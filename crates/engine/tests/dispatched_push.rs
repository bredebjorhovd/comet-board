//! gh#68: a dispatched agent's run carries the board's push credentials.
//!
//! The half that had no coverage before this change is the last hop — the one
//! that made #58's machinery a module with no callers. What is asserted here is
//! what the harness child would actually be spawned with: askpass wiring for
//! `git push`, a `gh` wrapper on PATH for `gh pr create`, the repo the attempt
//! belongs to, and no token anywhere in either.
//!
//! gh#107 added the second thing that rides the same hop: `GIT_AUTHOR_*` for
//! whoever released the work, so a teammate's dispatch commits as the teammate
//! while the box stays the committer.
//!
//! gh#184 added the third, and it rides a wider one: the directory holding the
//! `comet-board` this engine shipped with goes on *every* run's PATH, not only
//! a dispatched one's — the skill telling agents to run board verbs is
//! installed for the whole box.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use comet_board::config::Paths;
use comet_engine::push_credentials::PushCredentials;
use comet_engine::{EngineCore, HarnessRegistry};
use comet_harness::{Harness, HarnessError, RunControls};
use comet_proto::{
    AgentEvent, ChatConfig, DoneStatus, GitAuthor, HarnessId, Model, ReasoningLevel, RunRequest,
    SandboxLevel, SteeringMode,
};

/// A harness that records the controls it was handed and ends the run. Keyed
/// by chat: the auto-titler drives runs of its own through the same registry,
/// and those are chat-less.
#[derive(Default)]
struct RecordingHarness {
    seen: Mutex<Vec<Recorded>>,
}

/// One run as the harness saw it: which chat, what it pushes with, and which
/// directories were put in front of its PATH.
#[derive(Clone)]
struct Recorded {
    chat_id: Option<String>,
    push: Option<comet_harness::PushCredentials>,
    bin_dirs: Vec<std::path::PathBuf>,
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
            .push(Recorded {
                chat_id: controls.chat_id.clone(),
                push: controls.push.clone(),
                bin_dirs: controls.bin_dirs.clone(),
            });
        Ok(futures::stream::iter(vec![Ok(AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: None,
        })])
        .boxed())
    }
}

/// A steerable harness that keeps its run alive. The regression below never
/// lets a follow-up reach this mailbox; holding the receiver open proves the
/// engine took the live-steer path rather than the new-run fallback.
struct HoldingHarness;

#[async_trait]
impl Harness for HoldingHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Holding"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
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
        Ok(
            futures::stream::unfold(controls.steering, |mut steering| async move {
                let message = steering.recv().await?;
                Some((
                    Ok(AgentEvent::Steered {
                        assistant_message_id: None,
                        next_assistant_message_id: message.message_id,
                    }),
                    steering,
                ))
            })
            .boxed(),
        )
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

fn chat_config(push_repo: Option<&str>, git_author: Option<GitAuthor>) -> ChatConfig {
    ChatConfig {
        harness: HarnessId::Mock,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        sandbox: SandboxLevel::WorkspaceWrite,
        account: None,
        push_repo: push_repo.map(str::to_string),
        push_contract: push_repo.map(|_| comet_proto::GithubPushContract {
            contents_write: true,
            workflows_write: true,
        }),
        git_author,
        turn_limits: Default::default(),
        mcp_servers: Vec::new(),
    }
}

/// Write the config value exactly as an older/remote CRDT client would. This
/// deliberately bypasses WorkspaceHost's typed mutation seam so regressions in
/// document-side reconciliation remain observable.
fn replace_raw_config(core: &EngineCore, chat_id: &str, config: &ChatConfig) {
    let workspace = core.workspace.doc();
    let chats = workspace.doc().get_map("chats");
    let row = match chats.get(chat_id) {
        Some(loro::ValueOrContainer::Container(loro::Container::Map(row))) => row,
        other => panic!("missing chat row {chat_id}: {other:?}"),
    };
    row.insert(
        "config",
        loro::LoroValue::from(serde_json::to_value(config).unwrap()),
    )
    .unwrap();
    workspace.doc().commit();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dispatched_chats_run_carries_the_boards_credentials_and_a_plain_one_does_not() {
    let tmp = tempfile::Builder::new()
        .prefix("comet-gh68-")
        .tempdir()
        .expect("scratch dir");
    let dir = tmp.path();
    let data = dir.join("data");
    std::fs::create_dir_all(&data).unwrap();

    // Name the helper binary outright (gh#184): the engine resolves the
    // directory it puts on every child's PATH from it, and a test that let the
    // lookup fall through to PATH would assert something different on a box
    // with `comet-board` installed than on one without.
    //
    // A working stand-in rather than an empty file: since gh#233 the engine
    // *runs* the credential path before handing it to a run, so a `comet-board`
    // that cannot answer `git-askpass` is — correctly — no credential at all.
    let board_exe = dir.join("comet-board");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        &board_exe,
        "#!/bin/sh\n[ \"$1\" = git-askpass ] || exit 2\necho x-access-token\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&board_exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // SAFETY: single-test binary, set before the engine that reads it exists.
    unsafe { std::env::set_var("COMET_BOARD_EXECUTABLE", &board_exe) };

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
            Some(chat_config(
                Some("owner/widget"),
                Some(GitAuthor {
                    name: "Ana Ruiz".into(),
                    email: "22494697+ana@users.noreply.github.com".into(),
                }),
            )),
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

    // A dispatch by somebody the `[users]` map names, on a board with no repo
    // to authenticate for (a Linear ticket in a space with no GitHub remote):
    // the two halves are independent, so the commits are still theirs.
    core.workspace
        .create_chat(
            "chat-authored",
            "space-1",
            Some(chat_config(
                None,
                Some(GitAuthor {
                    name: "Sam Ito".into(),
                    email: "8134+samito@users.noreply.github.com".into(),
                }),
            )),
            Some("/tmp".into()),
        )
        .unwrap();
    core.sessions
        .dispatch("chat-authored", HarnessId::Mock, run_request("/tmp"), None)
        .await
        .expect("run dispatches");

    // A chat somebody opened themselves: no repo, no credentials, and the
    // agent keeps pushing with whatever git the box has.
    core.workspace
        .create_chat(
            "chat-plain",
            "space-1",
            Some(chat_config(None, None)),
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
        let chats: Vec<_> = recorded().into_iter().filter_map(|r| r.chat_id).collect();
        if ["chat-dispatched", "chat-authored", "chat-plain"]
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
    let run_for = |chat: &str| {
        seen.iter()
            .find(|r| r.chat_id.as_deref() == Some(chat))
            .cloned()
            .unwrap_or_else(|| panic!("no run reached the harness for {chat}"))
    };
    let for_chat = |chat: &str| run_for(chat).push;

    let push = for_chat("chat-dispatched").expect("the dispatched run has credentials");
    let env: std::collections::BTreeMap<_, _> = push.env.iter().cloned().collect();
    // gh#233: a path, naming a file git can exec, and nothing else. The
    // subcommand rides inside that file — it cannot ride in the variable,
    // because git execs `GIT_ASKPASS` rather than running it through a shell.
    let askpass = env.get("GIT_ASKPASS").expect("askpass wiring").clone();
    assert_eq!(
        askpass,
        paths
            .state_dir
            .join("bin")
            .join(comet_board::git_credentials::ASKPASS_SHIM)
            .display()
            .to_string()
    );
    assert!(
        std::path::Path::new(&askpass).is_file(),
        "GIT_ASKPASS names nothing git could exec: {askpass}"
    );
    assert!(
        std::fs::read_to_string(&askpass)
            .unwrap()
            .contains("git-askpass"),
        "the helper's subcommand went missing"
    );
    assert_eq!(
        env.get("COMET_BOARD_ASKPASS_REPO").map(String::as_str),
        Some("owner/widget")
    );
    assert_eq!(
        env.get(comet_board::git_credentials::PUSH_CONTRACT_ENV)
            .map(String::as_str),
        Some("contents+workflows")
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

    // `gh` gets a required wrapper on PATH rather than a token in the
    // environment. A dispatched GitHub run is refused if this wrapper cannot
    // be installed, so it may never fall through to the box's ambient login.
    let bin_dir = push.bin_dir.as_ref().expect("the required gh wrapper");
    let script = std::fs::read_to_string(bin_dir.join("gh")).unwrap();
    assert!(script.contains("gh-token"), "{script}");
    assert!(!script.contains("ghp_secret"), "{script}");
    assert!(
        !env.contains_key("GH_TOKEN"),
        "a token was exported into the agent's environment: {env:?}"
    );
    let git_guard = std::fs::read_to_string(bin_dir.join("git")).unwrap();
    assert!(git_guard.contains("COMET_BOARD_CHAT_ID='chat-dispatched'"), "{git_guard}");
    assert!(git_guard.contains("GIT_ASKPASS="), "{git_guard}");
    assert!(git_guard.contains("credential.helper"), "{git_guard}");

    // Whose commits these are (gh#107). The author is the teammate the board
    // resolved at dispatch; the committer is left unset, so git falls back to
    // the box's own pinned identity — "they wrote it, this box committed it".
    assert_eq!(
        env.get("GIT_AUTHOR_NAME").map(String::as_str),
        Some("Ana Ruiz")
    );
    assert_eq!(
        env.get("GIT_AUTHOR_EMAIL").map(String::as_str),
        Some("22494697+ana@users.noreply.github.com")
    );
    assert!(
        !env.keys().any(|k| k.starts_with("GIT_COMMITTER")),
        "the box commits: {env:?}"
    );

    let authored = for_chat("chat-authored").expect("an author with no repo is still an author");
    let env: std::collections::BTreeMap<_, _> = authored.env.iter().cloned().collect();
    assert_eq!(
        env.get("GIT_AUTHOR_EMAIL").map(String::as_str),
        Some("8134+samito@users.noreply.github.com")
    );
    assert!(
        !env.contains_key("GIT_ASKPASS"),
        "there is no repo to authenticate for: {env:?}"
    );

    assert!(
        for_chat("chat-plain").is_none(),
        "a chat the board never dispatched was handed credentials"
    );

    // A repo-only replacement that disagrees with the immutable shadow remains
    // corruption. Recovery is limited to a matching shadow or a true legacy
    // row whose minimum contract the host proves separately.
    let valid = chat_config(Some("owner/widget"), None);
    core.workspace
        .create_chat(
            "chat-missing-contract",
            "space-1",
            Some(valid.clone()),
            Some("/tmp".into()),
        )
        .unwrap();
    let mut inconsistent = valid;
    inconsistent.push_contract = None;
    inconsistent.push_repo = Some("other/widget".into());
    replace_raw_config(&core, "chat-missing-contract", &inconsistent);
    let error = core
        .sessions
        .dispatch(
            "chat-missing-contract",
            HarnessId::Mock,
            run_request("/tmp"),
            None,
        )
        .await
        .expect_err("an inconsistent GitHub push tuple reached the harness")
        .to_string();
    assert!(
        error.contains("conflicts with the board-owned push tuple"),
        "{error}"
    );
    assert!(
        !recorded()
            .iter()
            .any(|run| run.chat_id.as_deref() == Some("chat-missing-contract")),
        "a chat with no durable contract reached the harness"
    );

    // gh#184: every run — the dispatched one, the authored one, and the chat
    // the board never touched — can type `comet-board`. The failure this
    // replaces was silent: an agent that cannot reach the board does not stop,
    // it just gets on with the ticket without checking `dispatchable`,
    // releasing sub-work through the board, or waiting for it.
    for chat in ["chat-dispatched", "chat-authored", "chat-plain"] {
        assert_eq!(
            run_for(chat).bin_dirs,
            vec![dir.to_path_buf()],
            "{chat} could not have run comet-board"
        );
    }

    // The grant preflight may have passed earlier, but the handoff is resolved
    // again for the actual run. If the board credential disappears in between,
    // a board-dispatched GitHub chat refuses instead of reaching the harness
    // with ambient box credentials.
    std::fs::remove_file(paths.config_dir.join(".env")).unwrap();
    core.workspace
        .create_chat(
            "chat-broken-handoff",
            "space-1",
            Some(chat_config(Some("owner/widget"), None)),
            Some("/tmp".into()),
        )
        .unwrap();
    let error = core
        .sessions
        .dispatch(
            "chat-broken-handoff",
            HarnessId::Mock,
            run_request("/tmp"),
            None,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("credential handoff"), "{error}");
    assert!(
        !recorded()
            .iter()
            .any(|run| run.chat_id.as_deref() == Some("chat-broken-handoff")),
        "broken handoff reached the harness"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pre_contract_chat_is_upgraded_before_the_turn_and_never_uses_ambient_auth() {
    let tmp = tempfile::Builder::new()
        .prefix("comet-gh494-legacy-push-")
        .tempdir()
        .expect("scratch dir");
    let dir = tmp.path();
    let data = dir.join("data");
    std::fs::create_dir_all(&data).unwrap();

    let board_exe = dir.join("comet-board");
    std::fs::write(
        &board_exe,
        "#!/bin/sh\n[ \"$1\" = git-askpass ] || exit 2\necho x-access-token\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&board_exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let paths = Paths {
        config_dir: data.join("board"),
        state_dir: data.join("board/state"),
    };
    std::fs::create_dir_all(&paths.config_dir).unwrap();
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    std::fs::write(paths.config_dir.join(".env"), "GITHUB_TOKEN=ghp_secret\n").unwrap();

    let harness = Arc::new(RecordingHarness::default());
    let registry = HarnessRegistry::new();
    registry.register(harness.clone());
    let core = EngineCore::assemble(&data, Arc::new(registry), HarnessId::Mock, None)
        .expect("engine core assembles");
    core.sessions
        .set_push_credentials(Arc::new(PushCredentials::with_board_exe(
            paths.clone(),
            Some(board_exe),
        )));
    core.workspace
        .create_space(
            "space-legacy",
            &core.device_id,
            "/tmp",
            Some("widget".into()),
            true,
        )
        .unwrap();
    core.workspace
        .create_chat(
            "chat-legacy",
            "space-legacy",
            Some(chat_config(None, None)),
            Some("/tmp".into()),
        )
        .unwrap();
    let mut legacy = chat_config(None, None);
    legacy.push_repo = Some("owner/widget".into());
    replace_raw_config(&core, "chat-legacy", &legacy);

    core.sessions
        .dispatch("chat-legacy", HarnessId::Mock, run_request("/tmp"), None)
        .await
        .expect("a proven legacy chat dispatches");

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if harness
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|run| run.chat_id.as_deref() == Some("chat-legacy") && run.push.is_some())
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "legacy run never reached the harness with board credentials"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let recovered = core
        .workspace
        .github_push_state("chat-legacy")
        .unwrap()
        .expect("legacy state persisted");
    assert!(recovered.contract.contents_write);
    assert!(!recovered.contract.workflows_write);

    core.workspace
        .create_chat(
            "chat-unproved",
            "space-legacy",
            Some(chat_config(None, None)),
            Some("/tmp".into()),
        )
        .unwrap();
    replace_raw_config(&core, "chat-unproved", &legacy);
    std::fs::remove_file(paths.config_dir.join(".env")).unwrap();
    let error = core
        .sessions
        .dispatch("chat-unproved", HarnessId::Mock, run_request("/tmp"), None)
        .await
        .expect_err("an unproved legacy chat fell through to ambient auth")
        .to_string();
    assert!(error.contains("replacement push contract"), "{error}");
    assert_eq!(
        core.workspace
            .legacy_github_push_repo("chat-unproved")
            .unwrap()
            .as_deref(),
        Some("owner/widget")
    );
    core.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_the_board_credential_refuses_a_live_steer_before_writing_its_prompt() {
    let tmp = tempfile::Builder::new()
        .prefix("comet-gh440-live-steer-")
        .tempdir()
        .expect("scratch dir");
    let dir = tmp.path();
    let data = dir.join("data");
    std::fs::create_dir_all(&data).unwrap();

    let board_exe = dir.join("comet-board");
    std::fs::write(
        &board_exe,
        "#!/bin/sh\n[ \"$1\" = git-askpass ] || exit 2\necho x-access-token\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&board_exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let paths = Paths {
        config_dir: data.join("board"),
        state_dir: data.join("board/state"),
    };
    std::fs::create_dir_all(&paths.config_dir).unwrap();
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    std::fs::write(paths.config_dir.join(".env"), "GITHUB_TOKEN=ghp_secret\n").unwrap();

    let registry = HarnessRegistry::new();
    registry.register(Arc::new(HoldingHarness));
    let core = EngineCore::assemble(&data, Arc::new(registry), HarnessId::Mock, None)
        .expect("engine core assembles");
    core.sessions
        .set_push_credentials(Arc::new(PushCredentials::with_board_exe(
            paths.clone(),
            Some(board_exe.clone()),
        )));

    core.workspace
        .create_space(
            "space-live",
            &core.device_id,
            "/tmp",
            Some("widget".into()),
            true,
        )
        .unwrap();
    core.workspace
        .create_chat(
            "chat-live",
            "space-live",
            Some(chat_config(Some("owner/widget"), None)),
            Some("/tmp".into()),
        )
        .unwrap();

    // Simulate a config editor that predates the push fields, then persist and
    // restart before the next turn. The board-owned shadow must survive even
    // if the replaceable config value was the last thing written.
    let mut uninformed = chat_config(None, None);
    uninformed.model = Some("changed-before-reload".into());
    replace_raw_config(&core, "chat-live", &uninformed);
    core.workspace.flush();
    core.shutdown().await;
    drop(core);

    let registry = HarnessRegistry::new();
    registry.register(Arc::new(HoldingHarness));
    let core = EngineCore::assemble(&data, Arc::new(registry), HarnessId::Mock, None)
        .expect("engine core reloads");
    core.sessions
        .set_push_credentials(Arc::new(PushCredentials::with_board_exe(
            paths.clone(),
            Some(board_exe),
        )));
    let push = core
        .workspace
        .github_push_state("chat-live")
        .unwrap()
        .expect("reloaded chat remains board-dispatched");
    assert_eq!(push.repo, "owner/widget");
    assert!(push.contract.workflows_write);
    assert_eq!(
        core.workspace
            .chat_config("chat-live")
            .unwrap()
            .github_push_state()
            .unwrap(),
        Some(push)
    );
    core.sessions
        .dispatch("chat-live", HarnessId::Mock, run_request("/tmp"), None)
        .await
        .expect("initial run dispatches");

    // Repeat the uninformed replacement while the harness is live. Even if a
    // steer races document reconciliation, credential validation reads the
    // immutable tuple and cannot classify the chat as ordinary.
    uninformed.model = Some("changed-during-run".into());
    replace_raw_config(&core, "chat-live", &uninformed);
    std::fs::remove_file(paths.config_dir.join(".env")).unwrap();
    let error = core
        .sessions
        .steer(
            "chat-live",
            "do the workflow follow-up",
            Some("steer-2".into()),
        )
        .await
        .expect_err("a live steer survived credential removal")
        .to_string();
    assert!(error.contains("credential handoff"), "{error}");

    let entries = core
        .doc_host
        .open("chat-live")
        .unwrap()
        .doc()
        .read_entries()
        .unwrap();
    assert!(
        !entries.iter().flat_map(|entry| &entry.parts).any(|part| {
            matches!(part, comet_doc::MessagePart::Text { text, .. } if text.contains("workflow follow-up"))
        }),
        "the refused steer was written into the transcript"
    );

    core.sessions.shutdown().await;
}
