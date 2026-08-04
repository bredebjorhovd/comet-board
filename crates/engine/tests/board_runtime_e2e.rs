//! H2 end-to-end: `CometRuntime` (the board's `Runtime` trait against engine
//! internals) driven through a real assembled engine with the mock harness.
//!
//! dispatch → worktree cut on the spec's branch, chat created in the space,
//! brief queued and *executed*; prompt → a second turn; cancel → interrupt +
//! archive, which `chat_alive` then denies. The trait is sync and `dispatch`
//! blocks on async engine calls, so trait calls run on `spawn_blocking` —
//! exactly the off-runtime placement the board loop gives them in production.

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use comet_board::runtime::{DispatchSpec, RunEnd, Runtime};
use comet_doc::{MessageRole, MessageStatus};
use comet_engine::{CometRuntime, EngineCore, HarnessRegistry};
use comet_harness::mock::MockHarness;
use comet_proto::{AgentEvent, DoneStatus, HarnessId, SessionStatus};

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

fn mock_script() -> Vec<AgentEvent> {
    vec![
        AgentEvent::SessionStarted {
            harness: HarnessId::Mock,
            model: "mock-1".into(),
            tools: vec![],
            cwd: "/tmp".into(),
            session_id: "hs-1".into(),
            assistant_message_id: "a-1".into(),
        },
        AgentEvent::TextDelta {
            text: "done".into(),
        },
        AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("hs-1".into()),
        },
    ]
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

#[tokio::test(flavor = "multi_thread")]
async fn dispatch_prompt_cancel_against_a_real_engine() {
    let dir = tempfile::tempdir().unwrap();
    // Keep worktrees inside the tempdir — the default root is global
    // (`~/.comet-native/worktrees`) and a leftover checkout there would
    // collide with every later run. Safe to set: this binary has one test.
    unsafe { std::env::set_var("COMET_WORKTREES_DIR", dir.path().join("worktrees")) };
    let repo = dir.path().join("widget");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.email", "t@t"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["commit", "--allow-empty", "-m", "root"]);

    let registry = HarnessRegistry::new();
    registry.register(Arc::new(MockHarness {
        script: mock_script(),
    }));
    let core = EngineCore::assemble(
        &dir.path().join("data"),
        Arc::new(registry),
        HarnessId::Mock,
        None,
    )
    .unwrap();
    core.workspace
        .create_space(
            "space-widget",
            &core.device_id,
            &repo.to_string_lossy(),
            None,
            true,
        )
        .unwrap();

    let runtime = Arc::new(CometRuntime::new(
        core.repos.clone(),
        core.workspace.clone(),
        core.doc_host.clone(),
        core.workspace
            .merged_sessions_watch(core.sessions.watch_sessions()),
        core.sessions.journal(),
        core.agent_accounts.clone(),
        tokio::runtime::Handle::current(),
    ));

    // ── dispatch ────────────────────────────────────────────────────────────
    let spec = DispatchSpec {
        identifier: "gh#1".into(),
        space_id: "space-widget".into(),
        device_id: core.device_id.clone(),
        repo_path: repo.to_string_lossy().into_owned(),
        branch: "board/gh-1-widget".into(),
        worktree: true,
        harness: HarnessId::Mock,
        model: None,
        account: None,
        prompt: "do the thing".into(),
    };
    let rt = runtime.clone();
    let handle = tokio::task::spawn_blocking(move || rt.dispatch(&spec))
        .await
        .unwrap()
        .expect("dispatch succeeds");

    // The checkout is a fresh worktree on exactly the spec's branch.
    assert_ne!(handle.cwd, repo.to_string_lossy());
    let head = Command::new("git")
        .args(["-C", &handle.cwd, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&head.stdout).trim(),
        "board/gh-1-widget"
    );

    // The chat row: in the space, at the worktree, named for the task.
    let chat = core
        .workspace
        .doc()
        .chat(&handle.chat_id)
        .unwrap()
        .expect("chat exists");
    assert_eq!(chat.cwd.as_deref(), Some(handle.cwd.as_str()));
    assert_eq!(chat.branch.as_deref(), Some("board/gh-1-widget"));
    assert_eq!(chat.title.as_deref(), Some("gh#1"));
    assert_eq!(chat.space_id.as_deref(), Some("space-widget"));

    // The brief is not just queued — the host executor runs it.
    let doc = core.doc_host.open(&handle.chat_id).unwrap();
    let complete_turns = |doc: &comet_engine::ChatDocHandle| {
        doc.doc()
            .read_entries()
            .unwrap_or_default()
            .iter()
            .filter(|e| {
                e.role == MessageRole::Assistant && e.status == Some(MessageStatus::Complete)
            })
            .count()
    };
    wait_for(|| complete_turns(&doc) == 1, "the brief to execute").await;

    // The brief is VISIBLE in the transcript as the first user message — the
    // task's title + body land in the chat as the run's opening prompt, not
    // only in some internal queue (the report that a dispatched chat "showed
    // no prompt" is a crashed serve / un-executed command, not a missing send).
    let entries = doc.doc().read_entries().unwrap_or_default();
    let first_user_text = entries
        .iter()
        .filter(|e| e.role == MessageRole::User)
        .find_map(|e| {
            e.parts.iter().find_map(|p| match p {
                comet_doc::MessagePart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
        });
    assert_eq!(first_user_text, Some("do the thing"));

    // The session mirror answers for the chat, and settles back to Idle.
    let rt = runtime.clone();
    let chat_id = handle.chat_id.clone();
    wait_for(
        || {
            rt.session(&chat_id)
                .unwrap()
                .is_some_and(|s| s.status == SessionStatus::Idle)
        },
        "the session row to settle Idle",
    )
    .await;
    assert!(runtime.chat_alive(&handle.chat_id).unwrap());
    // The journal fact §H4's settle logic reads: the turn's `Done` is the
    // chat's last journaled event, and it completed.
    assert_eq!(
        runtime.last_run_end(&handle.chat_id).unwrap(),
        Some(RunEnd::Completed)
    );

    // ── an account that does not resolve refuses the dispatch (gh#59) ───────
    //
    // Before the chat exists, not at the first run: a chat holding an attempt
    // whose login is unknown is a row somebody has to clean up, and the caller
    // finds out either way.
    let chats_before = core.workspace.doc().read_chats().unwrap().len();
    let rt = runtime.clone();
    let bogus = DispatchSpec {
        identifier: "gh#2".into(),
        space_id: "space-widget".into(),
        device_id: core.device_id.clone(),
        repo_path: repo.to_string_lossy().into_owned(),
        branch: "board/gh-2-widget".into(),
        worktree: true,
        harness: HarnessId::ClaudeCode,
        model: None,
        account: Some("ffffffffffffffff".into()),
        prompt: "should never be sent".into(),
    };
    let err = tokio::task::spawn_blocking(move || rt.dispatch(&bogus))
        .await
        .unwrap()
        .expect_err("an unknown account refuses the dispatch");
    let err = format!("{err:#}");
    assert!(err.contains("ffffffffffffffff"), "{err}");
    assert_eq!(
        core.workspace.doc().read_chats().unwrap().len(),
        chats_before,
        "a refused dispatch leaves no chat behind"
    );

    // ── prompt (idle chat → a send, which runs a second turn) ───────────────
    runtime
        .prompt(&handle.chat_id, "and a follow-up")
        .expect("prompt queues");
    wait_for(|| complete_turns(&doc) == 2, "the follow-up to execute").await;

    // ── cancel (interrupt + archive) ────────────────────────────────────────
    runtime.cancel(&handle.chat_id).expect("cancel");
    let chat = core
        .workspace
        .doc()
        .chat(&handle.chat_id)
        .unwrap()
        .expect("chat row remains");
    assert!(chat.archived, "cancel archives the chat");
    assert!(
        !runtime.chat_alive(&handle.chat_id).unwrap(),
        "an archived chat is not alive"
    );

    core.shutdown().await;
}
