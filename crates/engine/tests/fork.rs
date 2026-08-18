//! gh#425 — forking a conversation from any message.
//!
//! The four things a fork has to get right, end to end against an assembled
//! engine: which destination it landed in (shared checkout vs its own
//! worktree), what it honestly carries (the provider's session vs a copy of the
//! visible transcript), that a failure reclaims exactly the checkout it cut,
//! and that a fork of a board attempt cannot pass for that attempt.

use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use comet_doc::{MessageRole, SessionCommandPayload};
use comet_engine::{EngineCore, HarnessRegistry};
use comet_harness::{Harness, HarnessError, RunControls};
use comet_proto::{
    AgentEvent, ChatConfig, DoneStatus, ForkCheckout, ForkContext, ForkRequest, HarnessId, Model,
    ReasoningLevel, RunRequest, SandboxLevel, SteeringMode,
};
use comet_rpc::methods;

const SPACE: &str = "space-widget";
const SOURCE: &str = "chat-source";

fn git(repo: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-C", &repo.to_string_lossy()])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn branch_of(cwd: &str) -> String {
    let out = Command::new("git")
        .args(["-C", cwd, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .expect("git runs");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A harness that keeps every request it was handed — the only way to see what
/// actually reached the agent, as opposed to what the transcript shows.
struct RecordingHarness {
    seen: Arc<Mutex<Vec<RunRequest>>>,
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
        request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.clone());
        let cwd = request.cwd.clone();
        Ok(futures::stream::iter(vec![
            Ok(AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "mock-1".into(),
                tools: vec![],
                cwd,
                session_id: "hs-fork".into(),
                assistant_message_id: "a-1".into(),
            }),
            Ok(AgentEvent::TextDelta {
                text: "on it".into(),
            }),
            Ok(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some("hs-fork".into()),
            }),
        ])
        .boxed())
    }
}

/// A harness whose turn remains steerable until the test tears the engine down.
struct PersistentHarness {
    seen: Arc<Mutex<Vec<RunRequest>>>,
}

#[async_trait]
impl Harness for PersistentHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Persistent"
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
        request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.clone());
        Ok(futures::stream::once(async move {
            Ok(AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "mock-1".into(),
                tools: vec![],
                cwd: request.cwd,
                session_id: "hs-persistent".into(),
                assistant_message_id: "a-persistent".into(),
            })
        })
        .chain(futures::stream::pending())
        .boxed())
    }
}

struct RejectingResumeHarness {
    seen: Arc<Mutex<Vec<RunRequest>>>,
}

#[async_trait]
impl Harness for RejectingResumeHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Rejecting resume"
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
        request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.clone());
        if request.resume.as_deref() == Some("hs-rejected") {
            return Ok(futures::stream::iter(vec![Ok(AgentEvent::Done {
                status: DoneStatus::Errored,
                result: None,
                error: Some("unknown session".into()),
                session_id: None,
            })])
            .boxed());
        }
        Ok(futures::stream::iter(vec![
            Ok(AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "mock-1".into(),
                tools: vec![],
                cwd: request.cwd,
                session_id: "hs-valid-destination".into(),
                assistant_message_id: "a-valid".into(),
            }),
            Ok(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some("hs-valid-destination".into()),
            }),
        ])
        .boxed())
    }
}

struct Rig {
    _env_guard: tokio::sync::OwnedMutexGuard<()>,
    _dir: tempfile::TempDir,
    core: EngineCore,
    repo: std::path::PathBuf,
    seen: Arc<Mutex<Vec<RunRequest>>>,
}

/// An engine over a real one-commit repo, with a space and a source chat
/// holding a three-message transcript.
async fn test_env() -> (tokio::sync::OwnedMutexGuard<()>, tempfile::TempDir) {
    static ENV_GUARD: std::sync::OnceLock<Arc<tokio::sync::Mutex<()>>> = std::sync::OnceLock::new();
    let env_guard = ENV_GUARD
        .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
        .lock_owned()
        .await;
    let dir = tempfile::tempdir().unwrap();
    // The default worktrees root is global (`~/.comet-native/worktrees`); a
    // fork test that cut one there would leave it on the box.
    unsafe { std::env::set_var("COMET_WORKTREES_DIR", dir.path().join("worktrees")) };
    (env_guard, dir)
}

async fn rig() -> Rig {
    let (env_guard, dir) = test_env().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let harness = Arc::new(RecordingHarness { seen: seen.clone() });
    rig_with_harness(env_guard, dir, seen, harness).await
}

async fn rig_with_harness(
    env_guard: tokio::sync::OwnedMutexGuard<()>,
    dir: tempfile::TempDir,
    seen: Arc<Mutex<Vec<RunRequest>>>,
    harness: Arc<dyn Harness>,
) -> Rig {
    let repo = dir.path().join("widget");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.email", "t@t"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["commit", "--allow-empty", "-m", "root"]);

    let registry = HarnessRegistry::new();
    registry.register(harness);
    let core = EngineCore::assemble(
        &dir.path().join("data"),
        Arc::new(registry),
        HarnessId::Mock,
        None,
    )
    .unwrap();
    core.repos.add(&repo.to_string_lossy()).await.unwrap();
    core.workspace
        .create_space(SPACE, &core.device_id, &repo.to_string_lossy(), None, true)
        .unwrap();
    core.workspace
        .create_chat(
            SOURCE,
            SPACE,
            Some(ChatConfig {
                harness: HarnessId::Mock,
                model: Some("mock-1".into()),
                reasoning: Some(ReasoningLevel::Medium),
                model_options: serde_json::Map::from_iter([(
                    "temperature".into(),
                    serde_json::json!(0.2),
                )]),
                sandbox: SandboxLevel::WorkspaceWrite,
                account: None,
                // The three things a board dispatch stamps and a fork must
                // never inherit.
                push_repo: Some("o/widget".into()),
                push_contract: Some(comet_proto::GithubPushContract {
                    contents_write: true,
                    workflows_write: true,
                }),
                git_author: Some(comet_proto::GitAuthor {
                    name: "Ana".into(),
                    email: "ana@example.invalid".into(),
                }),
                turn_limits: comet_proto::TurnLimits {
                    tool_failures: Some(3),
                    tool_calls: Some(30),
                },
                mcp_servers: vec![comet_proto::McpServer {
                    name: "route-only".into(),
                    command: "false".into(),
                    args: Vec::new(),
                }],
            }),
            Some(repo.to_string_lossy().into_owned()),
        )
        .unwrap();
    core.workspace
        .rename_chat(SOURCE, "Fix the parser")
        .unwrap();

    // A transcript to fork from, written the way the host writes one.
    let handle = core.doc_host.open(SOURCE).unwrap();
    handle
        .write_user_message("m1", "fix the parser", 1_000)
        .unwrap();
    handle
        .write_user_message("m2", "the lexer looks wrong", 2_000)
        .unwrap();
    handle
        .write_user_message("m3", "try again please", 3_000)
        .unwrap();
    // …and the provider session that transcript belongs to.
    core.workspace
        .set_chat_harness_session(SOURCE, "hs-source", &repo.to_string_lossy());

    Rig {
        _env_guard: env_guard,
        _dir: dir,
        core,
        repo,
        seen,
    }
}

fn deps<'a>(core: &'a EngineCore) -> comet_engine::ForkDeps<'a> {
    comet_engine::ForkDeps {
        workspace: &core.workspace,
        doc_host: &core.doc_host,
        repos: &core.repos,
        handoff: core.sessions.handoff(),
        default_harness: HarnessId::Mock,
    }
}

fn request(checkout: ForkCheckout) -> ForkRequest {
    ForkRequest {
        chat_id: SOURCE.into(),
        message_id: None,
        checkout,
        harness: None,
        model: None,
        account: None,
        title: None,
        prompt: None,
    }
}

async fn wait_for<F>(mut predicate: F, what: &str)
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

async fn staged_chat_id(gate: &std::path::Path) -> String {
    loop {
        if let Some(path) = std::fs::read_dir(gate)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|ext| ext == "reached"))
        {
            return path.file_stem().unwrap().to_string_lossy().into_owned();
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A tail fork still gets an independent conversation. A provider resume id
/// is mutable session ownership, not a clone operation, so context is copied.
#[tokio::test(flavor = "multi_thread")]
async fn a_shared_fork_at_the_tail_copies_into_an_independent_session() {
    let rig = rig().await;
    let result = comet_engine::fork_chat(deps(&rig.core), &request(ForkCheckout::Shared))
        .await
        .expect("fork lands");

    assert_eq!(result.lineage.context, ForkContext::TranscriptCopy);
    assert_eq!(
        result.lineage.copy_reason,
        Some(comet_proto::CopyReason::ProviderCloneUnavailable)
    );
    assert_eq!(
        std::fs::canonicalize(&result.cwd).unwrap(),
        std::fs::canonicalize(&rig.repo).unwrap()
    );
    assert!(result.note().contains("visible transcript"));

    let fork = rig
        .core
        .workspace
        .doc()
        .chat(&result.chat_id)
        .unwrap()
        .expect("fork row");
    assert_eq!(fork.harness_session_id, None);
    assert_eq!(fork.space_id.as_deref(), Some(SPACE));
    assert_eq!(fork.title.as_deref(), Some("Fix the parser (fork)"));
    let lineage = fork.forked_from.expect("lineage on the row");
    assert_eq!(lineage.source_chat_id, SOURCE);
    assert_eq!(lineage.checkout, ForkCheckout::Shared);
    assert_eq!(lineage.carried_messages, Some(3));

    // The copy is visible to the reader and waiting for the first prompt.
    let entries = rig
        .core
        .doc_host
        .open(&result.chat_id)
        .unwrap()
        .doc()
        .read_entries()
        .unwrap();
    assert_eq!(entries.len(), 1, "one system handoff: {entries:?}");

    // A fork is a chat a person opened: no push credential, no git author, no
    // unattended-run guardrails, whatever the source was carrying.
    let config = fork.config.expect("config");
    assert_eq!(config.push_repo, None);
    assert_eq!(config.push_contract, None);
    assert_eq!(config.git_author, None);
    assert!(config.turn_limits.is_off());
    assert!(config.mcp_servers.is_empty());
    assert_eq!(config.model.as_deref(), Some("mock-1"), "model inherited");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_explicit_id_equal_to_the_tail_is_still_a_tail_fork() {
    let rig = rig().await;
    let mut request = request(ForkCheckout::Shared);
    request.message_id = Some("m3".into());
    let result = comet_engine::fork_chat(deps(&rig.core), &request)
        .await
        .unwrap();
    assert_eq!(
        result.lineage.copy_reason,
        Some(comet_proto::CopyReason::ProviderCloneUnavailable)
    );
    let entries = rig
        .core
        .doc_host
        .open(&result.chat_id)
        .unwrap()
        .doc()
        .read_entries()
        .unwrap();
    let rendered = format!("{:?}", entries[0].parts);
    assert!(rendered.contains("the end of it"), "{rendered}");
    assert!(!rendered.contains("an earlier point"), "{rendered}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_concurrent_run_cannot_cross_the_staged_precommit_boundary() {
    let rig = rig().await;
    let gate = rig._dir.path().join("precommit-gate");
    unsafe { std::env::set_var("COMET_TEST_FORK_PRECOMMIT_GATE", &gate) };
    let fork_request = ForkRequest {
        prompt: Some("queued first run".into()),
        ..request(ForkCheckout::Shared)
    };
    let create = comet_engine::fork_chat(deps(&rig.core), &fork_request);
    let observe = async {
        let chat_id = staged_chat_id(&gate).await;
        assert!(rig.core.workspace.doc().chat(&chat_id).unwrap().is_some());
        assert!(
            rig.seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "the durable optional command has not drained before commit"
        );
        let error = rig
            .core
            .sessions
            .dispatch(
                &chat_id,
                HarnessId::Mock,
                RunRequest {
                    prompt: "concurrent remote run".into(),
                    model: None,
                    reasoning: None,
                    model_options: Default::default(),
                    cwd: "/caller".into(),
                    sandbox: SandboxLevel::DangerFullAccess,
                    auto_approve: true,
                    resume: Some("caller-session".into()),
                    attachments: Vec::new(),
                },
                None,
            )
            .await
            .expect_err("staged authority is fail closed");
        assert!(
            error.to_string().contains("not durably committed"),
            "{error}"
        );
        std::fs::write(gate.join(format!("{chat_id}.release")), b"release").unwrap();
        chat_id
    };
    let (created, chat_id) = tokio::join!(create, observe);
    unsafe { std::env::remove_var("COMET_TEST_FORK_PRECOMMIT_GATE") };
    assert_eq!(created.unwrap().chat_id, chat_id);
    wait_for(
        || {
            !rig.seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        },
        "committed optional command",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_precommit_failure_rolls_back_the_queued_run_and_chat() {
    let rig = rig().await;
    let gate = rig._dir.path().join("rollback-gate");
    unsafe { std::env::set_var("COMET_TEST_FORK_PRECOMMIT_GATE", &gate) };
    let fork_request = ForkRequest {
        prompt: Some("must never run".into()),
        ..request(ForkCheckout::Shared)
    };
    let create = comet_engine::fork_chat(deps(&rig.core), &fork_request);
    let release = async {
        let chat_id = staged_chat_id(&gate).await;
        std::fs::write(gate.join(format!("{chat_id}.fail")), b"fail").unwrap();
        std::fs::write(gate.join(format!("{chat_id}.release")), b"release").unwrap();
        chat_id
    };
    let (created, chat_id) = tokio::join!(create, release);
    unsafe { std::env::remove_var("COMET_TEST_FORK_PRECOMMIT_GATE") };
    assert!(
        created
            .unwrap_err()
            .to_string()
            .contains("injected precommit")
    );
    assert!(rig.core.workspace.doc().chat(&chat_id).unwrap().is_none());
    assert!(
        rig.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn synced_policy_poison_is_reverified_immediately_before_commit() {
    let rig = rig().await;
    let gate = rig._dir.path().join("poison-before-commit");
    unsafe { std::env::set_var("COMET_TEST_FORK_PRECOMMIT_GATE", &gate) };
    let fork_request = request(ForkCheckout::Shared);
    let create = comet_engine::fork_chat(deps(&rig.core), &fork_request);
    let poison = async {
        let chat_id = staged_chat_id(&gate).await;
        let mut config = rig.core.workspace.chat_config(&chat_id).unwrap();
        config.sandbox = SandboxLevel::DangerFullAccess;
        rig.core
            .workspace
            .set_chat_config(&chat_id, &config)
            .unwrap();
        std::fs::write(gate.join(format!("{chat_id}.release")), b"release").unwrap();
        chat_id
    };
    let (created, chat_id) = tokio::join!(create, poison);
    unsafe { std::env::remove_var("COMET_TEST_FORK_PRECOMMIT_GATE") };
    assert!(created.unwrap_err().to_string().contains("changed before"));
    assert!(rig.core.workspace.doc().chat(&chat_id).unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn rpc_source_rehome_during_fork_is_observed_at_commit_and_rolls_back() {
    let rig = rig().await;
    let gate = rig._dir.path().join("source-host-before-commit");
    unsafe { std::env::set_var("COMET_TEST_FORK_PRECOMMIT_GATE", &gate) };
    let fork_request = request(ForkCheckout::Shared);
    let create = comet_engine::fork_chat(deps(&rig.core), &fork_request);
    let rehome = async {
        let chat_id = staged_chat_id(&gate).await;
        let client = comet_rpc::memory_client(rig.core.rpc_service());
        client
            .call(
                methods::MUTATE,
                serde_json::json!({
                    "op": "setChatHost", "chatId": SOURCE, "deviceId": "other-host"
                }),
            )
            .await
            .expect("source rehome lands before fork commit");
        std::fs::write(gate.join(format!("{chat_id}.release")), b"release").unwrap();
        chat_id
    };
    let (created, staged) = tokio::join!(create, rehome);
    unsafe { std::env::remove_var("COMET_TEST_FORK_PRECOMMIT_GATE") };
    let error = created.expect_err("a source moved off-host cannot finish forking here");
    assert!(error.to_string().contains("moved to other-host"), "{error}");
    assert!(rig.core.workspace.doc().chat(&staged).unwrap().is_none());
    assert_eq!(
        rig.core
            .workspace
            .doc()
            .chat(SOURCE)
            .unwrap()
            .unwrap()
            .device_id,
        "other-host"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rpc_cannot_rehome_a_committed_fork_outside_its_authority() {
    let rig = rig().await;
    let result = comet_engine::fork_chat(deps(&rig.core), &request(ForkCheckout::Shared))
        .await
        .unwrap();
    let client = comet_rpc::memory_client(rig.core.rpc_service());
    let error = client
        .call(
            methods::MUTATE,
            serde_json::json!({
                "op": "setChatHost", "chatId": result.chat_id, "deviceId": "other-host"
            }),
        )
        .await
        .expect_err("fork authority refuses host reassignment");
    assert!(error.to_string().contains("host is immutable"), "{error}");
    assert_eq!(
        rig.core
            .workspace
            .doc()
            .chat(&result.chat_id)
            .unwrap()
            .unwrap()
            .device_id,
        rig.core.device_id
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_fork_failure_cannot_reclaim_the_other_intent() {
    let rig = rig().await;
    let gate = rig._dir.path().join("concurrent-rollback-gate");
    unsafe { std::env::set_var("COMET_TEST_FORK_PRECOMMIT_GATE", &gate) };
    let first_request = request(ForkCheckout::Shared);
    let second_request = request(ForkCheckout::Shared);
    let first = comet_engine::fork_chat(deps(&rig.core), &first_request);
    let second = comet_engine::fork_chat(deps(&rig.core), &second_request);
    let control = async {
        loop {
            let mut ids: Vec<_> = std::fs::read_dir(&gate)
                .ok()
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "reached"))
                .map(|path| path.file_stem().unwrap().to_string_lossy().into_owned())
                .collect();
            ids.sort();
            ids.dedup();
            if ids.len() == 2 {
                std::fs::write(gate.join(format!("{}.fail", ids[0])), b"fail").unwrap();
                for id in &ids {
                    std::fs::write(gate.join(format!("{id}.release")), b"release").unwrap();
                }
                break ids;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    let (first, second, ids) = tokio::join!(first, second, control);
    unsafe { std::env::remove_var("COMET_TEST_FORK_PRECOMMIT_GATE") };
    let outcomes = [first, second];
    assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(outcomes.iter().filter(|result| result.is_err()).count(), 1);
    let survivor = outcomes.into_iter().find_map(Result::ok).unwrap();
    assert!(
        rig.core
            .workspace
            .doc()
            .chat(&survivor.chat_id)
            .unwrap()
            .is_some()
    );
    let failed = ids.into_iter().find(|id| id != &survivor.chat_id).unwrap();
    assert!(rig.core.workspace.doc().chat(&failed).unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_synced_row_cannot_widen_host_owned_fork_authority() {
    let rig = rig().await;
    let result = comet_engine::fork_chat(deps(&rig.core), &request(ForkCheckout::Shared))
        .await
        .expect("fork lands");
    let mut config = rig
        .core
        .workspace
        .chat_config(&result.chat_id)
        .expect("fork config");
    config.sandbox = SandboxLevel::WorkspaceWrite;
    config.account = Some("source-billing".into());
    rig.core
        .workspace
        .set_chat_config(&result.chat_id, &config)
        .unwrap();

    let error = rig
        .core
        .sessions
        .dispatch(
            &result.chat_id,
            HarnessId::Mock,
            RunRequest {
                prompt: "try widened row".into(),
                model: None,
                reasoning: None,
                model_options: Default::default(),
                cwd: result.cwd,
                sandbox: SandboxLevel::WorkspaceWrite,
                auto_approve: true,
                resume: Some("hs-source".into()),
                attachments: Vec::new(),
            },
            None,
        )
        .await
        .expect_err("a remotely widened fork row is refused");
    assert!(
        error.to_string().contains("immutable host authority"),
        "{error}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rename_archive_branch_and_runtime_caches_do_not_replace_fork_policy() {
    let rig = rig().await;
    let result = comet_engine::fork_chat(deps(&rig.core), &request(ForkCheckout::Shared))
        .await
        .unwrap();
    rig.core
        .workspace
        .rename_chat(&result.chat_id, "Renamed")
        .unwrap();
    rig.core
        .workspace
        .set_chat_archived(&result.chat_id, true)
        .unwrap();
    rig.core
        .workspace
        .set_chat_branch(&result.chat_id, "review/local")
        .unwrap();
    rig.core
        .workspace
        .set_chat_harness_session(&result.chat_id, "row-cache", &result.cwd);

    rig.core
        .sessions
        .dispatch(
            &result.chat_id,
            HarnessId::Mock,
            RunRequest {
                prompt: "policy remains host owned".into(),
                model: Some("caller-model".into()),
                reasoning: None,
                model_options: Default::default(),
                cwd: "/caller".into(),
                sandbox: SandboxLevel::DangerFullAccess,
                auto_approve: true,
                resume: Some("caller-resume".into()),
                attachments: Vec::new(),
            },
            None,
        )
        .await
        .unwrap();
    wait_for(
        || {
            !rig.seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        },
        "renamed fork run",
    )
    .await;
    let seen = rig
        .seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(seen[0].model.as_deref(), Some("mock-1"));
    assert_eq!(seen[0].sandbox, SandboxLevel::ReadOnly);
    assert!(!seen[0].auto_approve);
    assert_eq!(
        std::fs::canonicalize(&seen[0].cwd).unwrap(),
        std::fs::canonicalize(&result.cwd).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_steer_revalidates_the_registered_checkout() {
    let (env_guard, dir) = test_env().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let rig = rig_with_harness(
        env_guard,
        dir,
        seen.clone(),
        Arc::new(PersistentHarness { seen: seen.clone() }),
    )
    .await;
    let result = comet_engine::fork_chat(deps(&rig.core), &request(ForkCheckout::Shared))
        .await
        .unwrap();
    rig.core
        .sessions
        .dispatch(
            &result.chat_id,
            HarnessId::Mock,
            RunRequest {
                prompt: "start a steerable turn".into(),
                model: None,
                reasoning: None,
                model_options: Default::default(),
                cwd: result.cwd.clone(),
                sandbox: SandboxLevel::ReadOnly,
                auto_approve: false,
                resume: None,
                attachments: Vec::new(),
            },
            None,
        )
        .await
        .unwrap();
    wait_for(
        || {
            !seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        },
        "persistent fork run",
    )
    .await;

    std::fs::write(rig._dir.path().join("data/repos.json"), "[]").unwrap();
    let error = rig
        .core
        .sessions
        .steer(&result.chat_id, "direct steer", None)
        .await
        .expect_err("direct steer revalidates the registered checkout too");
    assert!(
        error.to_string().contains("no longer registered"),
        "{error}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deregistered_stored_checkout_path_is_refused() {
    let rig = rig().await;
    let result = comet_engine::fork_chat(deps(&rig.core), &request(ForkCheckout::Shared))
        .await
        .unwrap();
    std::fs::write(rig._dir.path().join("data/repos.json"), "[]").unwrap();
    let error = rig
        .core
        .sessions
        .dispatch(
            &result.chat_id,
            HarnessId::Mock,
            RunRequest {
                prompt: "do not trust the surviving path".into(),
                model: None,
                reasoning: None,
                model_options: Default::default(),
                cwd: result.cwd,
                sandbox: SandboxLevel::ReadOnly,
                auto_approve: false,
                resume: None,
                attachments: Vec::new(),
            },
            None,
        )
        .await
        .expect_err("registry proof is required on every turn");
    assert!(
        error.to_string().contains("no longer registered"),
        "{error}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_caller_cannot_substitute_another_registered_checkout() {
    let rig = rig().await;
    let result = comet_engine::fork_chat(deps(&rig.core), &request(ForkCheckout::Shared))
        .await
        .expect("fork lands");
    let other = rig
        .core
        .repos
        .create_worktree(&rig.repo, "HEAD")
        .await
        .unwrap();

    rig.core
        .sessions
        .dispatch(
            &result.chat_id,
            HarnessId::Mock,
            RunRequest {
                prompt: "do not enter the replacement".into(),
                model: None,
                reasoning: None,
                model_options: Default::default(),
                cwd: other.path.clone(),
                sandbox: SandboxLevel::ReadOnly,
                auto_approve: false,
                resume: None,
                attachments: Vec::new(),
            },
            None,
        )
        .await
        .expect("the run uses authority instead of the caller checkout");
    wait_for(
        || {
            !rig.seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        },
        "authority-bound run",
    )
    .await;
    let seen = rig
        .seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(
        std::fs::canonicalize(&seen[0].cwd).unwrap(),
        std::fs::canonicalize(&result.cwd).unwrap()
    );
    assert_ne!(
        std::fs::canonicalize(&seen[0].cwd).unwrap(),
        std::fs::canonicalize(&other.path).unwrap()
    );
}

/// An older message is a fork point, and it is one a provider session cannot be
/// picked up at — so the fork says copy, says why, and the copy reaches BOTH
/// the reader (a transcript message) and the agent (the first prompt).
#[tokio::test(flavor = "multi_thread")]
async fn a_fork_at_an_older_message_copies_the_visible_transcript() {
    let rig = rig().await;
    let result = comet_engine::fork_chat(
        deps(&rig.core),
        &ForkRequest {
            message_id: Some("m2".into()),
            prompt: Some("review this independently".into()),
            ..request(ForkCheckout::Shared)
        },
    )
    .await
    .expect("fork lands");

    assert_eq!(result.lineage.context, ForkContext::TranscriptCopy);
    assert_eq!(
        result.lineage.copy_reason,
        Some(comet_proto::CopyReason::NotTheTail)
    );
    assert_eq!(result.lineage.carried_messages, Some(2));
    assert!(result.note().contains("hidden provider state did not come"));

    // The reader's copy: a system message, not words this person typed.
    let entries = rig
        .core
        .doc_host
        .open(&result.chat_id)
        .unwrap()
        .doc()
        .read_entries()
        .unwrap();
    let carried = entries.first().expect("handoff message");
    assert_eq!(carried.role, MessageRole::System);
    let text = format!("{:?}", carried.parts);
    assert!(text.contains("fix the parser"), "{text}");
    assert!(text.contains("the lexer looks wrong"), "{text}");
    assert!(
        !text.contains("try again please"),
        "nothing after the fork point: {text}"
    );

    // The agent's copy: the same transcript, prefixed to the first prompt —
    // while the transcript shows only what the person actually said.
    let seen = rig.seen.clone();
    wait_for(
        || {
            !seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        },
        "the fork's first run to reach the harness",
    )
    .await;
    let prompt = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)[0]
        .prompt
        .clone();
    let first = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)[0]
        .clone();
    assert_eq!(first.reasoning, Some(ReasoningLevel::Medium));
    assert_eq!(
        first.model_options.get("temperature"),
        Some(&serde_json::json!(0.2))
    );
    assert!(prompt.starts_with("# Forked conversation"), "{prompt}");
    assert!(prompt.contains("the lexer looks wrong"));
    assert!(prompt.ends_with("review this independently"), "{prompt}");
    assert!(
        entries
            .iter()
            .chain(
                rig.core
                    .doc_host
                    .open(&result.chat_id)
                    .unwrap()
                    .doc()
                    .read_entries()
                    .unwrap()
                    .iter()
            )
            .filter(|e| e.role == MessageRole::User)
            .all(|e| !format!("{:?}", e.parts).contains("# Forked conversation")),
        "the user's own message is their words alone"
    );

    // Simulate a crash after SessionStarted was journaled but before the
    // handoff unlink reached disk. The journal acknowledgement must win on
    // the next turn, so the stale copy is cleared rather than repeated.
    rig.core
        .sessions
        .handoff()
        .unwrap()
        .write(&result.chat_id, "stale copied context")
        .unwrap();
    rig.core.workspace.set_chat_harness_session(
        &result.chat_id,
        "hs-synced-row-poison",
        &rig.repo.to_string_lossy(),
    );

    // And it prefixes exactly one prompt: the second turn is unadorned.
    rig.core
        .doc_host
        .queue_command(
            &result.chat_id,
            SessionCommandPayload::Run {
                request: RunRequest {
                    prompt: "and now the other file".into(),
                    model: None,
                    reasoning: None,
                    model_options: Default::default(),
                    cwd: "/caller/poisoned".into(),
                    sandbox: SandboxLevel::WorkspaceWrite,
                    auto_approve: true,
                    resume: Some("hs-source".into()),
                    attachments: Vec::new(),
                },
                message_id: "m-second".into(),
            },
        )
        .unwrap();
    wait_for(
        || {
            seen.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len()
                >= 2
        },
        "the fork's second run",
    )
    .await;
    let second = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)[1]
        .clone();
    assert_eq!(second.prompt, "and now the other file");
    assert_eq!(
        second.resume.as_deref(),
        Some("hs-fork"),
        "the destination journal wins over the synced row's session id"
    );
    assert_eq!(second.sandbox, SandboxLevel::ReadOnly);
    assert!(!second.auto_approve);
    assert_eq!(
        std::fs::canonicalize(&second.cwd).unwrap(),
        std::fs::canonicalize(&rig.repo).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_destination_resume_stays_rejected_after_engine_restart() {
    let (env_guard, dir) = test_env().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let rig = rig_with_harness(
        env_guard,
        dir,
        seen.clone(),
        Arc::new(RejectingResumeHarness { seen: seen.clone() }),
    )
    .await;
    let data = rig._dir.path().join("data");
    let result = comet_engine::fork_chat(deps(&rig.core), &request(ForkCheckout::Shared))
        .await
        .unwrap();
    let journal = rig.core.sessions.journal();
    journal
        .append(
            &result.chat_id,
            &AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "mock-1".into(),
                tools: vec![],
                cwd: result.cwd.clone(),
                session_id: "hs-rejected".into(),
                assistant_message_id: "a-rejected".into(),
            },
        )
        .unwrap();
    journal
        .append(
            &result.chat_id,
            &AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some("hs-rejected".into()),
            },
        )
        .unwrap();
    rig.core
        .sessions
        .dispatch(
            &result.chat_id,
            HarnessId::Mock,
            RunRequest {
                prompt: "resume or recover".into(),
                model: None,
                reasoning: None,
                model_options: Default::default(),
                cwd: result.cwd.clone(),
                sandbox: SandboxLevel::ReadOnly,
                auto_approve: false,
                resume: None,
                attachments: Vec::new(),
            },
            None,
        )
        .await
        .unwrap();
    wait_for(
        || {
            seen.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len()
                >= 2
        },
        "rejected resume followed by fresh destination session",
    )
    .await;
    wait_for(
        || {
            journal.replay(&result.chat_id, 0).unwrap().iter().any(|(_, event)| {
                matches!(event, AgentEvent::Done { session_id: Some(id), .. } if id == "hs-valid-destination")
            })
        },
        "valid destination session to reach the journal",
    )
    .await;
    assert_eq!(
        seen.lock().unwrap()[0].resume.as_deref(),
        Some("hs-rejected")
    );
    assert_eq!(seen.lock().unwrap()[1].resume, None);
    drop(rig.core);

    let registry = HarnessRegistry::new();
    registry.register(Arc::new(RecordingHarness { seen: seen.clone() }));
    let restarted = EngineCore::assemble(&data, Arc::new(registry), HarnessId::Mock, None).unwrap();
    restarted
        .sessions
        .dispatch(
            &result.chat_id,
            HarnessId::Mock,
            RunRequest {
                prompt: "after restart".into(),
                model: None,
                reasoning: None,
                model_options: Default::default(),
                cwd: result.cwd,
                sandbox: SandboxLevel::ReadOnly,
                auto_approve: false,
                resume: None,
                attachments: Vec::new(),
            },
            None,
        )
        .await
        .unwrap();
    wait_for(
        || {
            seen.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len()
                >= 3
        },
        "post-restart destination resume",
    )
    .await;
    let requests = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(requests[2].resume.as_deref(), Some("hs-valid-destination"));
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.resume.as_deref() == Some("hs-rejected"))
            .count(),
        1,
        "the rejected id is never resurrected after reload"
    );
}

/// A different harness cannot pick up another one's session, whatever else
/// lines up — and the fork says so in those words.
#[tokio::test(flavor = "multi_thread")]
async fn a_fork_onto_another_harness_copies_and_says_why() {
    let rig = rig().await;
    let result = comet_engine::fork_chat(
        deps(&rig.core),
        &ForkRequest {
            harness: Some(HarnessId::ClaudeCode),
            ..request(ForkCheckout::Shared)
        },
    )
    .await
    .expect("fork lands");

    assert_eq!(result.lineage.context, ForkContext::TranscriptCopy);
    assert_eq!(
        result.lineage.copy_reason,
        Some(comet_proto::CopyReason::DifferentHarness)
    );
    let fork = rig
        .core
        .workspace
        .doc()
        .chat(&result.chat_id)
        .unwrap()
        .unwrap();
    let config = fork.config.expect("config");
    assert_eq!(config.harness, HarnessId::ClaudeCode);
    assert_eq!(
        config.model, None,
        "another harness's model is not this one's"
    );
    assert_eq!(fork.harness_session_id, None, "no session to inherit");
}

/// The isolated destination: its own worktree, its own branch, cut from where
/// the source stands — and never resumable, because provider session stores are
/// keyed by directory.
#[tokio::test(flavor = "multi_thread")]
async fn an_isolated_fork_gets_its_own_worktree_and_branch() {
    let rig = rig().await;
    let head_before = String::from_utf8_lossy(
        &Command::new("git")
            .args(["-C", &rig.repo.to_string_lossy(), "rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_string();

    let result = comet_engine::fork_chat(deps(&rig.core), &request(ForkCheckout::Isolated))
        .await
        .expect("fork lands");

    assert_ne!(result.cwd, rig.repo.to_string_lossy());
    assert_eq!(result.lineage.checkout, ForkCheckout::Isolated);
    assert_eq!(result.lineage.context, ForkContext::TranscriptCopy);
    assert_eq!(
        result.lineage.copy_reason,
        Some(comet_proto::CopyReason::DifferentCheckout)
    );
    let branch = result.branch.clone().expect("a branch of its own");
    assert!(branch.starts_with("comet/"), "{branch}");
    assert_eq!(branch_of(&result.cwd), branch);
    // Cut from where the source stands, not from wherever a branch has moved.
    let forked_head = String::from_utf8_lossy(
        &Command::new("git")
            .args(["-C", &result.cwd, "rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_string();
    assert_eq!(forked_head, head_before);
    // The source checkout is untouched.
    assert_eq!(branch_of(&rig.repo.to_string_lossy()), "main");
}

/// The atomicity criterion: a fork that fails after cutting a checkout leaves
/// neither the checkout nor its branch behind.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_isolated_fork_reclaims_the_worktree_it_cut() {
    let rig = rig().await;
    let before = rig.core.repos.branches(&rig.repo).await.unwrap();

    // An engine with no handoff store cannot carry a transcript, and every
    // isolated fork must — so this fails exactly after the worktree exists.
    let result = comet_engine::fork_chat(
        comet_engine::ForkDeps {
            handoff: None,
            ..deps(&rig.core)
        },
        &request(ForkCheckout::Isolated),
    )
    .await;
    assert!(result.is_err(), "the fork must fail");

    let after = rig.core.repos.branches(&rig.repo).await.unwrap();
    assert_eq!(before, after, "no branch survives a failed fork: {after:?}");
    let root = rig._dir.path().join("worktrees");
    let leftovers: Vec<_> = walk(&root)
        .into_iter()
        .filter(|p| p.join(".git").exists())
        .collect();
    assert!(leftovers.is_empty(), "no checkout survives: {leftovers:?}");
    let registered = String::from_utf8_lossy(
        &Command::new("git")
            .args([
                "-C",
                &rig.repo.to_string_lossy(),
                "worktree",
                "list",
                "--porcelain",
            ])
            .output()
            .unwrap()
            .stdout,
    )
    .to_string();
    assert_eq!(
        registered.matches("worktree ").count(),
        1,
        "only the primary checkout is registered: {registered}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unacknowledged_fork_is_reclaimed_when_the_engine_reseeds() {
    let rig = rig().await;
    let data = rig._dir.path().join("data");
    let worktree = rig
        .core
        .repos
        .create_worktree(&rig.repo, "HEAD")
        .await
        .unwrap();
    let chat_id = "chat-crashed-fork";
    let config = rig
        .core
        .workspace
        .doc()
        .chat(SOURCE)
        .unwrap()
        .unwrap()
        .config
        .unwrap();
    rig.core
        .workspace
        .create_chat(chat_id, SPACE, Some(config), Some(worktree.path.clone()))
        .unwrap();
    rig.core
        .doc_host
        .open(chat_id)
        .unwrap()
        .write_system_message("handoff", "copied", 1)
        .unwrap();
    rig.core.doc_host.persist_chat_snapshot(chat_id).unwrap();
    rig.core.workspace.flush_checked().unwrap();
    rig.core
        .sessions
        .handoff()
        .unwrap()
        .write(chat_id, "copied")
        .unwrap();
    let intent_dir = data.join("orgs/dev-org/dev-user/handoff");
    std::fs::write(
        intent_dir.join("chat-crashed-fork.fork.json"),
        serde_json::to_vec(&serde_json::json!({
            "chatId": chat_id,
            "repo": rig.repo,
            "worktree": worktree.path,
            "branch": worktree.branch,
            "committed": false
        }))
        .unwrap(),
    )
    .unwrap();
    let path = std::path::PathBuf::from(&worktree.path);
    let branch = worktree.branch.clone();
    let repo = rig.repo.clone();
    let seen = rig.seen.clone();
    drop(rig.core);

    let registry = HarnessRegistry::new();
    registry.register(Arc::new(RecordingHarness { seen }));
    let reseeded = EngineCore::assemble(&data, Arc::new(registry), HarnessId::Mock, None).unwrap();
    assert!(reseeded.workspace.doc().chat(chat_id).unwrap().is_none());
    assert!(!path.exists());
    let branch_still_exists = Command::new("git")
        .args([
            "-C",
            &repo.to_string_lossy(),
            "show-ref",
            "--verify",
            "--quiet",
        ])
        .arg(format!("refs/heads/{branch}"))
        .status()
        .unwrap()
        .success();
    assert!(!branch_still_exists);
    assert!(!intent_dir.join("chat-crashed-fork.fork.json").exists());
    assert!(!intent_dir.join("chat-crashed-fork.md").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_fork_is_not_republished_after_restart() {
    let rig = rig().await;
    let data = rig._dir.path().join("data");
    let result = comet_engine::fork_chat(deps(&rig.core), &request(ForkCheckout::Shared))
        .await
        .unwrap();
    rig.core.workspace.delete_chat(&result.chat_id).unwrap();
    rig.core.workspace.flush_checked().unwrap();
    let seen = rig.seen.clone();
    drop(rig.core);

    let registry = HarnessRegistry::new();
    registry.register(Arc::new(RecordingHarness { seen }));
    let restarted = EngineCore::assemble(&data, Arc::new(registry), HarnessId::Mock, None).unwrap();
    assert!(
        restarted
            .workspace
            .doc()
            .chat(&result.chat_id)
            .unwrap()
            .is_none()
    );
    let handoff = data.join("orgs/dev-org/dev-user/handoff");
    assert!(
        !handoff
            .join(format!("{}.authority.json", result.chat_id))
            .exists()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn committed_intent_unlink_failure_finishes_forward_on_restart() {
    let rig = rig().await;
    let data = rig._dir.path().join("data");
    unsafe {
        std::env::set_var("COMET_TEST_FORK_INTENT_UNLINK_FAIL", "*");
        std::env::set_var("COMET_TEST_FORK_ROLLBACK_FAIL", "*");
    }
    let result = comet_engine::fork_chat(deps(&rig.core), &request(ForkCheckout::Shared))
        .await
        .expect("post-commit cleanup never enters destructive rollback");
    unsafe {
        std::env::remove_var("COMET_TEST_FORK_INTENT_UNLINK_FAIL");
        std::env::remove_var("COMET_TEST_FORK_ROLLBACK_FAIL");
    }
    let intent = data
        .join("orgs/dev-org/dev-user/handoff")
        .join(format!("{}.fork.json", result.chat_id));
    assert!(intent.exists(), "committed cleanup remains retryable");
    let seen = rig.seen.clone();
    drop(rig.core);

    let registry = HarnessRegistry::new();
    registry.register(Arc::new(RecordingHarness { seen }));
    let restarted = EngineCore::assemble(&data, Arc::new(registry), HarnessId::Mock, None).unwrap();
    assert!(
        restarted
            .workspace
            .doc()
            .chat(&result.chat_id)
            .unwrap()
            .is_some()
    );
    assert!(
        !intent.exists(),
        "restart idempotently finishes committed cleanup"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rollback_failure_leaves_an_uncommitted_intent_restart_can_recover() {
    let rig = rig().await;
    let data = rig._dir.path().join("data");
    let gate = rig._dir.path().join("rollback-restart-gate");
    unsafe {
        std::env::set_var("COMET_TEST_FORK_PRECOMMIT_GATE", &gate);
        std::env::set_var("COMET_TEST_FORK_ROLLBACK_FAIL", "*");
    }
    let fork_request = request(ForkCheckout::Shared);
    let create = comet_engine::fork_chat(deps(&rig.core), &fork_request);
    let fail = async {
        let chat_id = staged_chat_id(&gate).await;
        std::fs::write(gate.join(format!("{chat_id}.fail")), b"fail").unwrap();
        std::fs::write(gate.join(format!("{chat_id}.release")), b"release").unwrap();
        chat_id
    };
    let (created, chat_id) = tokio::join!(create, fail);
    unsafe {
        std::env::remove_var("COMET_TEST_FORK_PRECOMMIT_GATE");
        std::env::remove_var("COMET_TEST_FORK_ROLLBACK_FAIL");
    }
    assert!(
        created
            .unwrap_err()
            .to_string()
            .contains("rollback failure")
    );
    assert!(rig.core.workspace.doc().chat(&chat_id).unwrap().is_some());
    let seen = rig.seen.clone();
    drop(rig.core);

    let registry = HarnessRegistry::new();
    registry.register(Arc::new(RecordingHarness { seen }));
    let restarted = EngineCore::assemble(&data, Arc::new(registry), HarnessId::Mock, None).unwrap();
    assert!(restarted.workspace.doc().chat(&chat_id).unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_shared_fork_reclaims_the_chat_and_doc() {
    let rig = rig().await;
    let before = rig.core.workspace.doc().read_chats().unwrap().len();
    let result = comet_engine::fork_chat(
        comet_engine::ForkDeps {
            handoff: None,
            ..deps(&rig.core)
        },
        &request(ForkCheckout::Shared),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(rig.core.workspace.doc().read_chats().unwrap().len(), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_over_limit_first_payload_is_refused_before_publication_or_run() {
    let rig = rig().await;
    let before = rig.core.workspace.doc().read_chats().unwrap().len();
    let error = comet_engine::fork_chat(
        deps(&rig.core),
        &ForkRequest {
            prompt: Some("x".repeat(32 * 1024)),
            ..request(ForkCheckout::Shared)
        },
    )
    .await
    .expect_err("context must not be silently dropped to fit the prompt");
    assert!(error.to_string().contains("32768 byte"), "{error}");
    assert_eq!(rig.core.workspace.doc().read_chats().unwrap().len(), before);
    assert!(
        rig.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty(),
        "no harness received the refused instruction"
    );
}

fn walk(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.push(path.clone());
            out.extend(walk(&path));
        }
    }
    out
}

/// The board-provenance criterion: a fork of an attempt's chat shares its
/// branch (shared checkout) and quotes its transcript, so on every signal the
/// adoption sweep reads it looks like the chat that did the work — and it must
/// never be offered as one. The attempt's review has to reach the conversation
/// that opened the pull request, not its second opinion.
#[tokio::test(flavor = "multi_thread")]
async fn a_fork_is_not_a_review_candidate() {
    let rig = rig().await;
    rig.core
        .workspace
        .set_chat_branch(SOURCE, "board/gh-1-widget")
        .unwrap();
    let result = comet_engine::fork_chat(deps(&rig.core), &request(ForkCheckout::Shared))
        .await
        .expect("fork lands");
    // The fork carries the same branch label a shared checkout implies.
    rig.core
        .workspace
        .set_chat_branch(&result.chat_id, "board/gh-1-widget")
        .unwrap();

    let runtime = comet_engine::CometRuntime::new(
        rig.core.repos.clone(),
        rig.core.workspace.clone(),
        rig.core.doc_host.clone(),
        rig.core
            .workspace
            .merged_sessions_watch(rig.core.sessions.watch_sessions()),
        rig.core.sessions.journal(),
        rig.core.agent_accounts.clone(),
        tokio::runtime::Handle::current(),
    );
    let candidates = tokio::task::spawn_blocking(move || {
        comet_board::runtime::Runtime::review_candidates(&runtime)
    })
    .await
    .unwrap()
    .expect("candidates read");
    assert!(
        candidates.iter().any(|c| c.chat_id == SOURCE),
        "the source chat is a candidate: {candidates:?}"
    );
    assert!(
        !candidates.iter().any(|c| c.chat_id == result.chat_id),
        "the fork is not: {candidates:?}"
    );
}

/// A fork point that is not in the transcript, and a chat that is not here at
/// all, are refusals rather than empty forks.
#[tokio::test(flavor = "multi_thread")]
async fn a_fork_point_that_is_not_a_message_is_refused() {
    let rig = rig().await;
    rig.core
        .doc_host
        .open(SOURCE)
        .unwrap()
        .write_system_message("system-complete", "host notice", 4_000)
        .unwrap();
    let err = comet_engine::fork_chat(
        deps(&rig.core),
        &ForkRequest {
            message_id: Some("system-complete".into()),
            ..request(ForkCheckout::Shared)
        },
    )
    .await
    .expect_err("system messages are not fork points");
    assert!(
        err.to_string().contains("completed user or assistant"),
        "{err}"
    );

    let err = comet_engine::fork_chat(
        deps(&rig.core),
        &ForkRequest {
            message_id: None,
            ..request(ForkCheckout::Shared)
        },
    )
    .await
    .expect_err("an implicit system tail is not a fork point");
    assert!(err.to_string().contains("transcript tail"), "{err}");

    let err = comet_engine::fork_chat(
        deps(&rig.core),
        &ForkRequest {
            message_id: Some("m-nope".into()),
            ..request(ForkCheckout::Shared)
        },
    )
    .await
    .expect_err("refused");
    assert!(err.to_string().contains("no message"), "{err}");

    let err = comet_engine::fork_chat(
        deps(&rig.core),
        &ForkRequest {
            chat_id: "chat-elsewhere".into(),
            ..request(ForkCheckout::Shared)
        },
    )
    .await
    .expect_err("refused");
    assert!(err.to_string().contains("no such chat"), "{err}");
}
