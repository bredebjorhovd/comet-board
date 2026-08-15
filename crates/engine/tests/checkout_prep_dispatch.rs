//! gh#422 end-to-end through a dispatch: a repository whose committed recipe
//! fails setup gets a checkout, a chat, and **no run** — and the retry that
//! fixes it releases the original brief into the same attempt.
//!
//! The trait is sync and `dispatch` blocks on async engine calls, so trait
//! calls run on `spawn_blocking` — the same off-runtime placement the board
//! loop gives them in production (see `board_runtime_e2e.rs`).

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use comet_board::runtime::{DispatchSpec, Runtime};
use comet_doc::{MessagePart, MessageRole};
use comet_engine::checkout_prep::{PrepState, RECIPE_PATH};
use comet_engine::{CometRuntime, EngineCore, HarnessRegistry};
use comet_harness::mock::MockHarness;
use comet_proto::{AgentEvent, DoneStatus, HarnessId, SessionStatus};
use comet_rpc::methods;

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
async fn a_failed_setup_holds_the_brief_and_a_retry_releases_it() {
    let dir = tempfile::tempdir().unwrap();
    // Keep worktrees inside the tempdir. Safe to set: this binary has one test.
    unsafe { std::env::set_var("COMET_WORKTREES_DIR", dir.path().join("worktrees")) };

    // An origin whose default branch COMMITS the recipe — a fresh worktree has
    // to arrive already carrying it, that being the whole point of the file.
    // Its setup fails, and counts how often it ran.
    let origin = dir.path().join("origin");
    std::fs::create_dir_all(origin.join(".comet")).unwrap();
    git(&origin, &["init", "-b", "main"]);
    git(&origin, &["config", "user.email", "t@t"]);
    git(&origin, &["config", "user.name", "t"]);
    std::fs::write(
        origin.join(RECIPE_PATH),
        "version = 1\n[setup]\nrun = \"echo ran >> prep-count.txt; echo 'toolchain missing' >&2; exit 7\"\n",
    )
    .unwrap();
    git(&origin, &["add", "."]);
    git(&origin, &["commit", "-m", "recipe"]);
    let repo = dir.path().join("widget");
    git(
        dir.path(),
        &["clone", &origin.to_string_lossy(), &repo.to_string_lossy()],
    );
    git(&repo, &["config", "user.email", "t@t"]);
    git(&repo, &["config", "user.name", "t"]);

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
        core.checkout_prep.clone(),
        tokio::runtime::Handle::current(),
    ));

    let spec = DispatchSpec {
        identifier: "gh#422".into(),
        title: "Prepare before you spend".into(),
        space_id: "space-widget".into(),
        device_id: core.device_id.clone(),
        repo_path: repo.to_string_lossy().into_owned(),
        branch: "board/gh-422-widget".into(),
        base: "origin/HEAD".into(),
        worktree: true,
        harness: HarnessId::Mock,
        model: None,
        account: None,
        push_repo: None,
        git_author: None,
        turn_limits: Default::default(),
        agent_instructions: false,
        prompt: "do the thing".into(),
    };
    let rt = runtime.clone();
    let handle = tokio::task::spawn_blocking(move || rt.dispatch(&spec))
        .await
        .unwrap()
        .expect("dispatch succeeds — preparation gates the run, not the checkout");

    // Preparation fails; the checkout, chat and attempt all survive it.
    let worktree = std::path::PathBuf::from(&handle.cwd);
    let prep = core.checkout_prep.clone();
    {
        let (prep, worktree) = (prep.clone(), worktree.clone());
        wait_for(
            move || {
                prep.status(&worktree)
                    .is_some_and(|r| r.state == PrepState::Failed)
            },
            "the prep record to say failed",
        )
        .await;
    }
    let record = prep.status(&worktree).unwrap();
    assert_eq!(record.exit_code, Some(7));
    assert!(worktree.exists(), "a failed preparation preserves the checkout");

    // The one property the issue is named for: no run started, nothing billed.
    // The brief is parked, not queued — the journal has no run and the session
    // row never appeared.
    assert!(prep.parked(&worktree).is_some(), "the brief is parked");
    assert!(
        core.sessions.session_status(&handle.chat_id).is_none(),
        "no run may start in an unprepared checkout"
    );
    // And the chat says why, in a message nobody typed.
    let entries = core
        .doc_host
        .open(&handle.chat_id)
        .unwrap()
        .doc()
        .read_entries()
        .unwrap();
    assert!(
        entries.iter().any(|e| e.role == MessageRole::System
            && e.parts.iter().any(|p| matches!(
                p,
                MessagePart::Error { message, .. } if message.contains("could not be prepared")
            ))),
        "the failure is written into the transcript"
    );

    // The operator fixes the recipe IN THE SAME CHECKOUT and retries over RPC
    // — the ordinary-session verb, exercised here because it is also the
    // board's recovery path.
    std::fs::write(
        worktree.join(RECIPE_PATH),
        "version = 1\n[setup]\nrun = \"echo ran >> prep-count.txt\"\n",
    )
    .unwrap();
    let client = comet_rpc::memory_client(core.rpc_service());
    let prepared = client
        .call(
            methods::PREPARE_CHECKOUT,
            serde_json::json!({ "worktreePath": handle.cwd }),
        )
        .await
        .expect("PrepareCheckout");
    assert_eq!(prepared["state"], "ready", "{prepared}");

    // The release: the SAME brief reaches the ledger, the mock run executes,
    // and the parked copy is gone — a second retry has nothing to double-send.
    assert!(prep.parked(&worktree).is_none(), "the brief was taken");
    {
        let (sessions, chat) = (core.sessions.clone(), handle.chat_id.clone());
        wait_for(
            move || {
                sessions
                    .session_status(&chat)
                    .is_some_and(|s| s.status == SessionStatus::Idle)
            },
            "the released brief to run to completion",
        )
        .await;
    }
    let count = std::fs::read_to_string(worktree.join("prep-count.txt")).unwrap();
    assert_eq!(
        count.lines().count(),
        2,
        "setup ran once per attempt at it — failed once, then once fixed"
    );

    // Same worktree, same branch, same attempt-shaped world: the retry cut
    // nothing new.
    let head = Command::new("git")
        .args(["-C", &handle.cwd, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&head.stdout).trim(),
        "board/gh-422-widget"
    );
}
