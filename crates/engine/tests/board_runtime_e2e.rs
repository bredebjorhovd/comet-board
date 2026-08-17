//! §runtime-impl end-to-end: `CometRuntime` (the board's `Runtime` trait
//! against engine internals) driven through a real assembled engine with the
//! mock harness.
//!
//! dispatch → worktree cut on the spec's branch, chat created in the space,
//! brief queued and *executed*; prompt → a second turn; cancel → interrupt +
//! archive, which `chat_alive` then denies. The trait is sync and `dispatch`
//! blocks on async engine calls, so trait calls run on `spawn_blocking` —
//! exactly the off-runtime placement the board loop gives them in production.

#[cfg(target_os = "linux")]
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(target_os = "linux")]
use std::net::TcpListener;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
use std::process::Command;
#[cfg(target_os = "linux")]
use std::process::Stdio;
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use comet_board::config::Paths;
#[cfg(target_os = "linux")]
use comet_board::credential_ledger;
#[cfg(target_os = "linux")]
use comet_board::db::{Db, UpsertTask};
#[cfg(target_os = "linux")]
use comet_board::dispatch::{DispatchOrigin, DispatchOverrides};
#[cfg(target_os = "linux")]
use comet_board::model::{BoardState, Outcome, Source, UpstreamState};
use comet_board::runtime::{DispatchSpec, RunEnd, Runtime};
#[cfg(target_os = "linux")]
use comet_board::sources::github::{CapabilityEvidence, PushCapabilities, WriteCapability};
use comet_doc::{MessageRole, MessageStatus};
#[cfg(target_os = "linux")]
use comet_engine::board::BoardService;
use comet_engine::push_credentials::PushCredentials;
use comet_engine::{CometRuntime, EngineCore, HarnessRegistry};
#[cfg(target_os = "linux")]
use comet_harness::codex::CodexHarness;
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

static WORKTREES_ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct WorktreesEnvGuard {
    _lock: tokio::sync::MutexGuard<'static, ()>,
    prior: Option<std::ffi::OsString>,
    prior_codex_home: Option<Option<std::ffi::OsString>>,
}

impl Drop for WorktreesEnvGuard {
    fn drop(&mut self) {
        if let Some(prior) = self.prior.take() {
            unsafe { std::env::set_var("COMET_WORKTREES_DIR", prior) };
        } else {
            unsafe { std::env::remove_var("COMET_WORKTREES_DIR") };
        }
        if let Some(prior) = self.prior_codex_home.take() {
            if let Some(prior) = prior {
                unsafe { std::env::set_var("CODEX_HOME", prior) };
            } else {
                unsafe { std::env::remove_var("CODEX_HOME") };
            }
        }
    }
}

impl WorktreesEnvGuard {
    #[cfg(target_os = "linux")]
    fn set_codex_home(&mut self, path: &std::path::Path) {
        self.prior_codex_home = Some(std::env::var_os("CODEX_HOME"));
        unsafe { std::env::set_var("CODEX_HOME", path) };
    }
}

async fn isolate_worktrees(path: &std::path::Path) -> WorktreesEnvGuard {
    let lock = WORKTREES_ENV.lock().await;
    let prior = std::env::var_os("COMET_WORKTREES_DIR");
    unsafe { std::env::set_var("COMET_WORKTREES_DIR", path) };
    WorktreesEnvGuard {
        _lock: lock,
        prior,
        prior_codex_home: None,
    }
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

#[cfg(target_os = "linux")]
fn authenticated_git_http(origin: PathBuf) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://x-access-token@{}/repo.git",
        listener.local_addr().unwrap()
    );
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            let mut content_length = 0usize;
            let mut content_type = String::new();
            let mut authenticated = false;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if let Some(value) = lower.strip_prefix("content-length:") {
                    content_length = value.trim().parse().unwrap();
                }
                if let Some(value) = lower.strip_prefix("content-type:") {
                    content_type = value.trim().to_string();
                }
                if let Some(value) = lower.strip_prefix("authorization: basic ") {
                    authenticated = value.trim() == "eC1hY2Nlc3MtdG9rZW46Ym9hcmQtdG9rZW4=";
                }
            }
            let mut body = vec![0; content_length];
            reader.read_exact(&mut body).unwrap();
            if !authenticated {
                write!(stream, "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=board\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                continue;
            }
            let target = request.split_whitespace().nth(1).unwrap();
            let (path, query) = target.split_once('?').unwrap_or((target, ""));
            let path_info = path.strip_prefix("/repo.git").unwrap_or("");
            let mut child = Command::new("git")
                .arg("http-backend")
                .env("GIT_PROJECT_ROOT", origin.parent().unwrap())
                .env("GIT_HTTP_EXPORT_ALL", "1")
                .env(
                    "PATH_INFO",
                    format!(
                        "/{}{}",
                        origin.file_name().unwrap().to_string_lossy(),
                        path_info
                    ),
                )
                .env("QUERY_STRING", query)
                .env("REQUEST_METHOD", request.split_whitespace().next().unwrap())
                .env("CONTENT_TYPE", content_type)
                .env("CONTENT_LENGTH", content_length.to_string())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.as_mut().unwrap().write_all(&body).unwrap();
            let output = child.wait_with_output().unwrap();
            let split = output
                .stdout
                .windows(2)
                .position(|w| w == b"\n\n")
                .map(|i| i + 2)
                .or_else(|| {
                    output
                        .stdout
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .map(|i| i + 4)
                })
                .unwrap();
            let headers = String::from_utf8_lossy(&output.stdout[..split]);
            let response_body = &output.stdout[split..];
            write!(stream, "HTTP/1.1 200 OK\r\n").unwrap();
            for header in headers.lines() {
                let header = header.trim_end_matches('\r');
                if !header.is_empty() {
                    write!(stream, "{header}\r\n").unwrap();
                }
            }
            write!(stream, "Connection: close\r\n\r\n").unwrap();
            stream.write_all(response_body).unwrap();
        }
    });
    url
}

#[cfg(target_os = "linux")]
fn serve_push_model(command: String) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let requests = Arc::new(AtomicUsize::new(0));
    let served = Arc::clone(&requests);
    std::thread::spawn(move || {
        for (index, stream) in listener.incoming().take(2).enumerate() {
            served.fetch_add(1, Ordering::SeqCst);
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let response = if index == 0 {
                let args = serde_json::json!({"cmd": command, "yield_time_ms": 10000}).to_string();
                format!(
                    "event: response.created\ndata: {{\"type\":\"response.created\",\"response\":{{\"id\":\"r1\"}}}}\n\nevent: response.output_item.done\ndata: {{\"type\":\"response.output_item.done\",\"item\":{{\"type\":\"function_call\",\"call_id\":\"push\",\"name\":\"exec_command\",\"arguments\":{}}}}}\n\nevent: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"r1\",\"usage\":{{\"input_tokens\":0,\"output_tokens\":0,\"total_tokens\":0}}}}}}\n\n",
                    serde_json::to_string(&args).unwrap()
                )
            } else {
                "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r2\"}}\n\nevent: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"id\":\"m1\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r2\",\"usage\":{\"input_tokens\":0,\"output_tokens\":0,\"total_tokens\":0}}}\n\n".into()
            };
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
        }
    });
    (base, requests)
}

async fn wait_for<F>(mut predicate: F, what: &str)
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
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
    // Keep worktrees inside the tempdir — the default root is global. The
    // guard serializes both tests in this binary and restores the caller's
    // process environment when the fixture drops.
    let _worktrees_env = isolate_worktrees(&dir.path().join("worktrees")).await;
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
    let push = Arc::new(PushCredentials::with_board_exe(
        paths.clone(),
        Some(board_exe),
    ));
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
        core.checkout_prep.clone(),
        tokio::runtime::Handle::current(),
    ));
    runtime.set_push_credentials(push);
    let push_contract = comet_proto::GithubPushContract {
        contents_write: true,
        workflows_write: true,
    };
    runtime
        .verify_push_credentials("o/widget", push_contract)
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
        push_contract: Some(push_contract),
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
    assert_eq!(
        chat.config.as_ref().and_then(|c| c.push_contract),
        Some(push_contract)
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
        push_contract: None,
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

    // The contract is checked again on a later board-delivered turn, before a
    // durable command or transcript entry is written. Rotating/removing the
    // credential after dispatch cannot leave the old brief's promise alive.
    std::fs::remove_file(paths.config_dir.join(".env")).unwrap();
    let error = runtime
        .prompt(&handle.chat_id, "an undeliverable review follow-up")
        .expect_err("a later turn survived credential removal")
        .to_string();
    assert!(error.contains("credential handoff"), "{error}");
    assert_eq!(complete_turns(&doc), 2);
    assert!(
        !doc.doc()
            .read_entries()
            .unwrap_or_default()
            .iter()
            .flat_map(|entry| &entry.parts)
            .any(|part| matches!(part, comet_doc::MessagePart::Text { text, .. } if text.contains("undeliverable review"))),
        "the refused later prompt reached the transcript"
    );

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
    // On `spawn_blocking` like every other trait call here (see the header):
    // since gh#422 the sweep may run the repo's own `[archive]` step first,
    // which blocks on the runtime handle exactly as `dispatch` does.
    let swept = {
        let rt = runtime.clone();
        let cwd = handle.cwd.clone();
        tokio::task::spawn_blocking(move || rt.reclaim_build_output(&cwd))
            .await
            .unwrap()
            .expect("the sweep runs")
    };
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

/// gh#488's whole production chain: the board releases the task, the real
/// runtime hands the chat its protected tool PATH, Codex issues the push after
/// deleting every inherited credential variable, and the board loop settles
/// only after that same chat has minted through askpass.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn board_dispatch_push_and_settle_share_one_guarded_credential_event() {
    use std::os::unix::fs::PermissionsExt;

    if !Command::new("codex")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        assert_ne!(
            std::env::var_os("COMET_REQUIRE_REAL_CODEX"),
            Some("1".into())
        );
        eprintln!("no real Codex CLI; skipping board dispatch regression");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut worktrees_env = isolate_worktrees(&dir.path().join("worktrees")).await;
    let source = dir.path().join("widget");
    std::fs::create_dir_all(&source).unwrap();
    git(&source, &["init", "-b", "main"]);
    git(&source, &["config", "user.email", "t@t"]);
    git(&source, &["config", "user.name", "t"]);
    std::fs::write(source.join("seed"), "seed\n").unwrap();
    git(&source, &["add", "seed"]);
    git(&source, &["commit", "-m", "root"]);
    let checkout_origin = dir.path().join("checkout-origin.git");
    git(
        dir.path(),
        &[
            "clone",
            "--bare",
            source.to_str().unwrap(),
            checkout_origin.to_str().unwrap(),
        ],
    );
    git(
        &source,
        &["remote", "add", "origin", checkout_origin.to_str().unwrap()],
    );

    let receive = dir.path().join("receive.git");
    git(dir.path(), &["init", "--bare", receive.to_str().unwrap()]);
    let output = Command::new("git")
        .args([
            "--git-dir",
            receive.to_str().unwrap(),
            "config",
            "http.receivepack",
            "true",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let push_url = authenticated_git_http(receive.clone());
    let (model, model_requests) = serve_push_model(format!(
        "unset GIT_ASKPASS COMET_BOARD_ASKPASS_REPO COMET_BOARD_CHAT_ID COMET_BOARD_PUSH_CONTRACT {} {} GIT_TERMINAL_PROMPT GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0; git push '{}' HEAD:refs/heads/board-test",
        concat!("COMET_BOARD_", "CONFIG_DIR"),
        concat!("COMET_BOARD_", "STATE_DIR"),
        push_url
    ));

    let data = dir.path().join("data");
    let paths = Paths::under(&data).unwrap();
    std::fs::create_dir_all(&paths.config_dir).unwrap();
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    std::fs::write(paths.config_dir.join(".env"), "GITHUB_TOKEN=configured\n").unwrap();
    let board = dir.path().join("comet-board");
    let ledger = credential_ledger::path(&paths);
    std::fs::write(&board, format!(
        "#!/bin/sh\n[ \"$1\" = git-askpass ] || exit 2\ncase \"$2\" in *sername*) echo x-access-token ;; *) printf '{{\"at\":\"test\",\"event\":\"minted\",\"tool\":\"git-askpass\",\"repo\":\"owner/widget\",\"chat\":\"%s\"}}\\n' \"$COMET_BOARD_CHAT_ID\" >> '{}'; echo board-token ;; esac\n",
        ledger.display()
    )).unwrap();
    std::fs::set_permissions(&board, std::fs::Permissions::from_mode(0o755)).unwrap();

    let home = dir.path().join("home");
    let codex_home = dir.path().join("codex-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&codex_home).unwrap();
    std::fs::write(
        codex_home.join("auth.json"),
        serde_json::json!({ "OPENAI_API_KEY": "fixture-key" }).to_string(),
    )
    .unwrap();
    worktrees_env.set_codex_home(&codex_home);
    let hostile = dir.path().join("ambient-used");
    let ambient = dir.path().join("ambient-helper");
    std::fs::write(&ambient, format!("#!/bin/sh\nprintf used > '{}'\n[ \"$1\" = get ] && printf 'username=x-access-token\\npassword=ambient-token\\n'\n", hostile.display())).unwrap();
    std::fs::set_permissions(&ambient, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        home.join(".gitconfig"),
        format!("[credential]\n\thelper = {}\n", ambient.display()),
    )
    .unwrap();
    std::fs::write(codex_home.join("config.toml"), format!(
        "model = \"gpt-5.4\"\nmodel_provider = \"mock\"\n[model_providers.mock]\nname = \"mock\"\nbase_url = \"{}\"\nwire_api = \"responses\"\nrequires_openai_auth = false\nrequest_max_retries = 0\nstream_max_retries = 0\n[features]\nunified_exec = true\nshell_snapshot = true\n[sandbox_workspace_write]\nnetwork_access = true\n[shell_environment_policy]\ninherit = \"none\"\ninclude_only = [\"HOME\"]\n",
        model
    )).unwrap();
    let codex = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|p| p.join("codex"))
        .find(|p| p.is_file())
        .unwrap();
    let live_codex = dir.path().join("live-codex");
    std::fs::write(
        &live_codex,
        format!(
            "#!/bin/sh\nexport CODEX_HOME='{}' HOME='{}'\nexec '{}' \"$@\"\n",
            codex_home.display(),
            home.display(),
            codex.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&live_codex, std::fs::Permissions::from_mode(0o755)).unwrap();

    let registry = HarnessRegistry::new();
    registry.register(Arc::new(CodexHarness::new().with_executable(live_codex)));
    let core = EngineCore::assemble(&data, Arc::new(registry), HarnessId::Codex, None).unwrap();
    core.workspace
        .create_space(
            "space-widget",
            &core.device_id,
            &source.to_string_lossy(),
            None,
            true,
        )
        .unwrap();
    let push = Arc::new(PushCredentials::with_board_exe(paths.clone(), Some(board)));
    core.sessions.set_push_credentials(push.clone());
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
    runtime.set_push_credentials(push);
    std::fs::write(paths.routing(), format!("[[route]]\nmatch = {{ gh_repo = \"owner/widget\" }}\nworkspace = \"widget\"\nrepo = {:?}\nruntime = \"codex\"\nbase = \"HEAD\"\n", source.to_string_lossy())).unwrap();
    let db = Db::open(&paths.db()).unwrap();
    db.upsert_task(&UpsertTask {
        id: "gh:owner/widget#488".into(),
        source: Source::Github,
        source_id: "488".into(),
        identifier: "gh#488".into(),
        title: "guard push".into(),
        body: Some("push it".into()),
        url: "https://github.com/owner/widget/issues/488".into(),
        labels: vec![],
        source_state: None,
        linear_team: None,
        linear_project: None,
        upstream: UpstreamState::Unstarted,
        updated_at: comet_board::db::now(),
    })
    .unwrap();
    drop(db);
    let board_service = BoardService::spawn_at_with_capabilities(
        paths.clone(),
        core.workspace
            .merged_sessions_watch(core.sessions.watch_sessions()),
        runtime.clone(),
        core.workspace.watch_spaces(),
        tokio::runtime::Handle::current(),
        Some(PushCapabilities {
            contents: WriteCapability::Write,
            workflows: WriteCapability::Write,
            evidence: CapabilityEvidence::AppInstallation,
        }),
    )
    .unwrap();
    let dispatched = board_service
        .dispatch_task(
            "gh:owner/widget#488",
            DispatchOrigin::default(),
            DispatchOverrides::default(),
        )
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let pushed = loop {
        if {
            Command::new("git")
                .args([
                    "--git-dir",
                    receive.to_str().unwrap(),
                    "show-ref",
                    "--verify",
                    "--quiet",
                    "refs/heads/board-test",
                ])
                .status()
                .unwrap()
                .success()
        } {
            break true;
        }
        if tokio::time::Instant::now() >= deadline {
            break false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert!(
        pushed,
        "model-issued remote ref missing; model requests={}; sessions={:?}; run_end={:?}; ledger={:?}",
        model_requests.load(Ordering::SeqCst),
        core.sessions.watch_sessions().borrow().clone(),
        runtime.last_run_end(&dispatched.chat_id),
        credential_ledger::for_chat(&paths, &dispatched.chat_id),
    );
    wait_for(
        || {
            let facts = credential_ledger::for_chat(&paths, &dispatched.chat_id);
            facts.handed && facts.minted
        },
        "matching Handed and Minted",
    )
    .await;
    assert!(!hostile.exists(), "ambient helper bypassed the board guard");
    let db = Db::open(&paths.db()).unwrap();
    db.set_pr(
        "gh:owner/widget#488",
        Some("https://github.com/owner/widget/pull/1"),
        Some(1),
        true,
    )
    .unwrap();
    drop(db);
    wait_for(
        || {
            Db::open(&paths.db())
                .unwrap()
                .get_task("gh:owner/widget#488")
                .unwrap()
                .is_some_and(|t| t.state == BoardState::Review)
        },
        "SyncEngine settlement",
    )
    .await;
    let task = Db::open(&paths.db())
        .unwrap()
        .get_task("gh:owner/widget#488")
        .unwrap()
        .unwrap();
    assert_eq!(
        task.attempts.last().and_then(|a| a.outcome),
        Some(Outcome::Done)
    );
    let attempt = task.attempts.last().unwrap();
    let credential_notice_key = format!("{}:credential:{}", task.id, attempt.id);
    let credential_notice_exists = Db::open(&paths.db())
        .unwrap()
        .has_writeback(&task.id, "credential", &credential_notice_key)
        .unwrap();
    assert!(
        !credential_notice_exists,
        "settlement classified the model push as an alternate credential"
    );
    board_service.shutdown();
    core.shutdown().await;
}
