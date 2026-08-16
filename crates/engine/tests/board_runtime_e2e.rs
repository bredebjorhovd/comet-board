//! §runtime-impl end-to-end: `CometRuntime` (the board's `Runtime` trait
//! against engine internals) driven through a real assembled engine with the
//! mock harness.
//!
//! dispatch → worktree cut on the spec's branch, chat created in the space,
//! brief queued and *executed*; prompt → a second turn; cancel → interrupt +
//! archive, which `chat_alive` then denies. The trait is sync and `dispatch`
//! blocks on async engine calls, so trait calls run on `spawn_blocking` —
//! exactly the off-runtime placement the board loop gives them in production.

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use comet_board::config::Paths;
use comet_board::runtime::{DispatchSpec, RunEnd, Runtime};
use comet_doc::{MessageRole, MessageStatus};
use comet_engine::push_credentials::PushCredentials;
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
    // A clone with an origin, because that is what a dispatch now needs: the
    // spec's `base` is fetched before the branch is cut (gh#67).
    let origin = dir.path().join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-b", "main"]);
    git(&origin, &["config", "user.email", "t@t"]);
    git(&origin, &["config", "user.name", "t"]);
    git(&origin, &["commit", "--allow-empty", "-m", "root"]);
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

    // A real board-enabled engine wires one resolver into both the dispatch
    // preflight and every later run. This test assembles the core by hand, so
    // provide the same explicit handoff rather than letting its dispatched
    // chat inherit whatever GitHub credential happens to exist on the runner.
    let board_exe = dir.path().join("comet-board");
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
        config_dir: dir.path().join("data/board"),
        state_dir: dir.path().join("data/board/state"),
    };
    std::fs::create_dir_all(&paths.config_dir).unwrap();
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    std::fs::write(paths.config_dir.join(".env"), "GITHUB_TOKEN=ghp_secret\n").unwrap();
    let push = Arc::new(PushCredentials::with_board_exe(paths, Some(board_exe)));
    core.sessions.set_push_credentials(push.clone());

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
    runtime.set_push_credentials(push);
    runtime
        .verify_push_credentials("o/widget")
        .expect("the dispatch preflight accepts the explicit handoff");

    // What the box user's own instruction files say before any of this
    // (gh#272). A dispatch naming no account writes into the CLI's own config
    // dir, and the only reason this test does not is that the mock harness
    // reads no such file — asserted after the dispatch, because the day that
    // stops being true is the day a test edits somebody's `~/.claude`.
    let untouched: Vec<(std::path::PathBuf, Option<String>)> =
        [HarnessId::ClaudeCode, HarnessId::Codex]
            .into_iter()
            .filter_map(|h| comet_board::conventions::user_config_dir(h).map(|d| (h, d)))
            .filter_map(|(h, d)| comet_board::conventions::path_in(&d, h))
            .map(|p| {
                let before = std::fs::read_to_string(&p).ok();
                (p, before)
            })
            .collect();

    // ── dispatch ────────────────────────────────────────────────────────────
    let spec = DispatchSpec {
        identifier: "gh#1".into(),
        title: "Teach the widget to fold".into(),
        space_id: "space-widget".into(),
        device_id: core.device_id.clone(),
        repo_path: repo.to_string_lossy().into_owned(),
        branch: "board/gh-1-widget".into(),
        base: "origin/HEAD".into(),
        worktree: true,
        harness: HarnessId::Mock,
        model: None,
        account: None,
        push_repo: Some("o/widget".into()),
        git_author: Some(comet_proto::GitAuthor {
            name: "Ana Ruiz".into(),
            email: "22494697+ana@users.noreply.github.com".into(),
        }),
        // The route's turn guardrails ride the spec (gh#270) and are stamped
        // on the chat, where the engine's run loop reads them.
        turn_limits: comet_proto::TurnLimits {
            tool_failures: Some(9),
            tool_calls: Some(900),
        },
        mcp_servers: vec![comet_proto::McpServer {
            name: "comet-board".into(),
            command: "comet-board".into(),
            args: vec!["mcp".into()],
        }],
        // On, as a real route's dispatch is (gh#272). The mock harness reads
        // no instruction file, so this one writes nothing — which is also what
        // keeps the test off the box user's own `~/.claude`.
        agent_instructions: true,
        prompt: "do the thing".into(),
    };
    let rt = runtime.clone();
    let handle = tokio::task::spawn_blocking(move || rt.dispatch(&spec))
        .await
        .unwrap()
        .expect("dispatch succeeds");

    for (path, before) in &untouched {
        assert_eq!(
            &std::fs::read_to_string(path).ok(),
            before,
            "a mock dispatch must not write into {}",
            path.display()
        );
    }

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
    // Identifier AND title: the sidebar row has to be readable without
    // looking the issue up.
    assert_eq!(
        chat.title.as_deref(),
        Some("gh#1 · Teach the widget to fold")
    );
    assert_eq!(chat.space_id.as_deref(), Some("space-widget"));
    // The repo its pushes authenticate for, on the chat rather than on the run
    // (gh#68): the fix for a review comment next week is a new run in this same
    // chat, and it has to reach the same branch.
    assert_eq!(
        chat.config.as_ref().and_then(|c| c.push_repo.as_deref()),
        Some("o/widget")
    );
    // And whose name its commits carry (gh#107), on the chat for the same
    // reason: that later fix should be by the same person as the first commit.
    assert_eq!(
        chat.config
            .as_ref()
            .and_then(|c| c.git_author.as_ref())
            .map(|a| a.email.as_str()),
        Some("22494697+ana@users.noreply.github.com")
    );
    assert_eq!(
        chat.config
            .as_ref()
            .map(|c| c.mcp_servers.as_slice())
            .unwrap_or_default(),
        [comet_proto::McpServer {
            name: "comet-board".into(),
            command: "comet-board".into(),
            args: vec!["mcp".into()],
        }]
    );

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
    // The journal fact §settle-logic reads: the turn's `Done` is the
    // chat's last journaled event, and it completed.
    assert_eq!(
        runtime.last_run_end(&handle.chat_id).unwrap(),
        Some(RunEnd::Completed)
    );
    // The other journal fact, the one the restart budget is spent against
    // (§gh#392): this chat has a journal here and boot recovery has revived it
    // nothing, which is a zero. A chat this device has never run is not a zero
    // — it is no answer at all, and the board reports the two differently.
    assert_eq!(runtime.chat_revivals(&handle.chat_id).unwrap(), Some(0));
    assert_eq!(runtime.chat_revivals("chat-somewhere-else").unwrap(), None);

    // ── an account that does not resolve refuses the dispatch (gh#59) ───────
    //
    // Before the chat exists, not at the first run: a chat holding an attempt
    // whose login is unknown is a row somebody has to clean up, and the caller
    // finds out either way.
    let chats_before = core.workspace.doc().read_chats().unwrap().len();
    let rt = runtime.clone();
    let bogus = DispatchSpec {
        identifier: "gh#2".into(),
        title: "A dispatch whose login does not resolve".into(),
        space_id: "space-widget".into(),
        device_id: core.device_id.clone(),
        repo_path: repo.to_string_lossy().into_owned(),
        branch: "board/gh-2-widget".into(),
        base: "origin/HEAD".into(),
        worktree: true,
        harness: HarnessId::ClaudeCode,
        model: None,
        account: Some("ffffffffffffffff".into()),
        push_repo: None,
        git_author: None,
        turn_limits: Default::default(),
        mcp_servers: Vec::new(),
        agent_instructions: true,
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

    // ── shelf (archive without interrupting, and back again) ────────────────
    // gh#139's verb: the retention sweep files a finished chat away a week
    // after its task leaves the board, and a re-opened attempt brings it back.
    // Both directions against a real workspace doc, because the whole point is
    // that this is the same mutation the sidebar's own Archive writes.
    let archived = |doc: &comet_doc::WorkspaceDoc| {
        doc.chat(&handle.chat_id)
            .unwrap()
            .expect("chat row remains")
            .archived
    };
    runtime
        .set_chat_archived(&handle.chat_id, true)
        .expect("archive");
    assert!(archived(&core.workspace.doc()), "the chat is off the shelf");
    assert!(
        !runtime.chat_alive(&handle.chat_id).unwrap(),
        "which is exactly why review delivery must never be archived out from under"
    );
    runtime
        .set_chat_archived(&handle.chat_id, false)
        .expect("unarchive");
    assert!(!archived(&core.workspace.doc()), "and back on it");
    assert!(runtime.chat_alive(&handle.chat_id).unwrap());

    // ── build output (gh#186): the cache goes, the checkout stays ───────────
    //
    // Against a real worktree, because that is the whole claim: the sweep must
    // leave a directory git still recognises, on its branch, with the agent's
    // work in it — the checkout is 14 MB of evidence and only the 36 GB of
    // `target/` inside it is a cache.
    let checkout = std::path::Path::new(&handle.cwd);
    std::fs::write(checkout.join("kept.rs"), "the agent's work").unwrap();
    for cache in ["target/debug", "web/node_modules/react"] {
        std::fs::create_dir_all(checkout.join(cache)).unwrap();
        std::fs::write(checkout.join(cache).join("blob"), vec![b'x'; 4096]).unwrap();
    }
    let swept = runtime
        .reclaim_build_output(&handle.cwd)
        .expect("the sweep runs");
    assert_eq!(swept.dirs, 2, "target/ and the nested node_modules");
    assert_eq!(swept.bytes, 8192);
    assert!(swept.failed.is_empty());
    assert!(!checkout.join("target").exists());
    assert!(!checkout.join("web").join("node_modules").exists());
    // Everything the attempt is retained *for*:
    assert_eq!(
        std::fs::read_to_string(checkout.join("kept.rs")).unwrap(),
        "the agent's work"
    );
    let head = Command::new("git")
        .args(["-C", &handle.cwd, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&head.stdout).trim(),
        "board/gh-1-widget",
        "a swept checkout is still a checkout on its branch"
    );
    // And the worktree is still registered with its repo — the sweep is not a
    // half-done `reclaim_worktree`.
    let listed = Command::new("git")
        .args(["-C", &repo.to_string_lossy(), "worktree", "list"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains("board/gh-1-widget"),
        "{}",
        String::from_utf8_lossy(&listed.stdout)
    );

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
