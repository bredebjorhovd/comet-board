//! CodexHarness integration tests, primarily against the fake app server in
//! `tests/fixtures/fake-codex.sh`. The push-policy regression uses a real
//! installed Codex app server because its recursive config merge and model
//! tool shell are the boundary under test. CI pins and requires that CLI.

#[cfg(unix)]
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::net::TcpListener;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};

#[cfg(unix)]
use comet_board::config::Paths;
#[cfg(unix)]
use comet_board::{credential_ledger, git_credentials};
use comet_harness::{
    CancellationToken, CodexHarness, Harness, HarnessError, PushCredentials, RunControls,
    SteerMessage,
};
use comet_proto::{
    AgentEvent, DoneStatus, HarnessId, ReasoningLevel, RunRequest, SandboxLevel, TodoItem,
    ToolCall, UserInputAnswer, UserInputQuestion,
};

mod common;

/// Ceiling on any single fake-CLI run, before [`common::scaled`]. It covers a
/// child spawn and teardown as well as the scenario, so it scales with the
/// runner (gh#167).
const RUN_DEADLINE: Duration = Duration::from_secs(10);

#[cfg(unix)]
fn authenticated_git_http(origin: PathBuf) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("git HTTP listener");
    let url = format!("http://x-access-token@{}/origin.git", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming().take(8) {
            let mut stream = stream.expect("git HTTP request");
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            let mut length = 0usize;
            let mut content_type = String::new();
            let mut authorized = false;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" { break; }
                let lower = line.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("content-length:") { length = v.trim().parse().unwrap(); }
                if let Some(v) = line.strip_prefix("Content-Type:").or_else(|| line.strip_prefix("content-type:")) { content_type = v.trim().into(); }
                if let Some(v) = line.strip_prefix("Authorization:").or_else(|| line.strip_prefix("authorization:")) {
                    let expected = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "x-access-token:board-token");
                    authorized = v.trim() == format!("Basic {expected}");
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            if !authorized {
                stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"GitHub\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                continue;
            }
            let target = first.split_whitespace().nth(1).unwrap();
            let (path, query) = target.split_once('?').unwrap_or((target, ""));
            let root = origin.parent().unwrap();
            let mut child = std::process::Command::new("git")
                .arg("http-backend")
                .env("GIT_PROJECT_ROOT", root)
                .env("GIT_HTTP_EXPORT_ALL", "1")
                .env("PATH_INFO", path)
                .env("QUERY_STRING", query)
                .env("REQUEST_METHOD", first.split_whitespace().next().unwrap())
                .env("REMOTE_USER", "x-access-token")
                .env("CONTENT_TYPE", content_type)
                .env("CONTENT_LENGTH", length.to_string())
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn().unwrap();
            child.stdin.take().unwrap().write_all(&body).unwrap();
            let out = child.wait_with_output().unwrap();
            let split = out.stdout.windows(2).position(|w| w == b"\n\n").map(|i| i + 2)
                .or_else(|| out.stdout.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)).unwrap();
            stream.write_all(b"HTTP/1.1 200 OK\r\n").unwrap();
            stream.write_all(&out.stdout[..split]).unwrap();
            stream.write_all(&out.stdout[split..]).unwrap();
            if std::process::Command::new("git").args(["--git-dir", origin.to_str().unwrap(), "show-ref", "--verify", "--quiet", "refs/heads/board-test"]).status().unwrap().success() { break; }
        }
    });
    url
}

fn fixture_path() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-codex.sh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    path
}

fn harness() -> CodexHarness {
    CodexHarness::new().with_executable(fixture_path())
}

fn request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        model: Some("gpt-5.6-sol".into()),
        reasoning: Some(ReasoningLevel::Ultra),
        model_options: serde_json::Map::new(),
        cwd: String::new(),
        sandbox: SandboxLevel::WorkspaceWrite,
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
    harness: &CodexHarness,
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
async fn happy_path_maps_deltas_items_usage_and_done() {
    let (controls, _steer, _token) = controls("Yes");
    let mut req = request("scenario:happy");
    req.cwd = "/tmp".into();
    req.model_options.insert(
        "serviceTier".into(),
        serde_json::Value::String("fast".into()),
    );
    let events = run_to_end(&harness(), req, controls).await;

    // SessionStarted from thread/start's thread id.
    let starts: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::SessionStarted {
                harness,
                model,
                cwd,
                session_id,
                ..
            } => Some((harness, model, cwd, session_id)),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 1, "{events:?}");
    let (h, model, cwd, session_id) = starts[0];
    assert_eq!(*h, HarnessId::Codex);
    assert_eq!(model, "gpt-5.6-sol");
    assert_eq!(cwd, "/tmp");
    assert_eq!(session_id, "th-1");

    // Deltas — both wire spellings accepted.
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "Hello".into()
    }));
    assert!(events.contains(&AgentEvent::ReasoningDelta {
        text: "thinking hard".into()
    }));
    assert!(events.contains(&AgentEvent::ReasoningDelta {
        text: "summary".into()
    }));

    // commandExecution: ToolCall at started only, exit code 1 => error result.
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolCall { id, .. } if id == "c1"))
            .count(),
        1
    );
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "c1".into(),
        call: ToolCall::Exec {
            command: "ls -la".into()
        },
    }));
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "c1".into(),
        is_error: true
    }));

    // fileChange (single add): WriteFile, refreshed at completion.
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(
                e,
                AgentEvent::ToolCall {
                    id,
                    call: ToolCall::WriteFile { path, content: None }
                } if id == "f1" && path == "/tmp/new.rs"
            ))
            .count(),
        2,
        "started + completion-refresh: {events:?}"
    );
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "f1".into(),
        is_error: false
    }));

    // mcpToolCall with failed status.
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "mcp1".into(),
        call: ToolCall::Mcp {
            server: "linear".into(),
            tool: "search".into(),
            input: Some(serde_json::json!({"q": "bug"})),
        },
    }));
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "mcp1".into(),
        is_error: true
    }));

    // webSearch lifecycle.
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "w1".into(),
        call: ToolCall::WebSearch {
            query: "rust".into()
        },
    }));
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "w1".into(),
        is_error: false
    }));

    // Completion-only todoList still opens and closes the lifecycle.
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "td1".into(),
        call: ToolCall::Todo {
            items: vec![
                TodoItem {
                    text: "a".into(),
                    done: true
                },
                TodoItem {
                    text: "b".into(),
                    done: false
                },
            ]
        },
    }));
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "td1".into(),
        is_error: false
    }));

    // Streamed agentMessage must not re-emit its completed text…
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta { text } if text == "Hello world")),
        "streamed message text re-emitted: {events:?}"
    );
    // …but a never-streamed one falls back to the completed text.
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "unstreamed tail".into()
    }));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::AssistantMessageCompleted { .. }))
            .count(),
        2
    );

    // Usage rides just before the terminal Done.
    let usage_pos = events
        .iter()
        .position(|e| {
            matches!(
                e,
                AgentEvent::Usage(comet_proto::TokenUsage {
                    input_tokens: 42,
                    output_tokens: 7,
                    ..
                })
            )
        })
        .expect("usage emitted");
    let done_pos = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Done { .. }))
        .expect("done emitted");
    assert!(usage_pos < done_pos);

    // Fullness is NOT held for the turn's end (gh#271): each snapshot that
    // moves the level is reported while the turn is still running, and the
    // straggler that arrives after `turn/completed` is dropped — a run journal
    // ending on anything but a `Done` reads as a run that died mid-stream.
    let levels: Vec<comet_proto::ContextUsage> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ContextUsage(usage) => Some(*usage),
            _ => None,
        })
        .collect();
    assert_eq!(levels.len(), 2, "one per move, none after Done: {levels:?}");
    assert_eq!(levels[0].used_tokens, 250_000);
    assert_eq!(levels[0].max_tokens, 272_000);
    assert_eq!(levels[0].percent(), Some(92));
    // Codex names no compaction point, so a ratio is all there is to judge on.
    assert!(levels[0].is_near_compaction(0.9));
    assert_eq!(levels[0].compact_at_tokens, None);
    // …and after the compaction the level is back down.
    assert_eq!(levels[1].percent(), Some(0));

    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("th-1".into()),
        })
    );
}

#[cfg(unix)]
#[tokio::test]
async fn spawned_codex_receives_the_route_mcp_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (wrapper, argv_file) = common::recording_wrapper(&fixture_path(), &dir);
    let harness = CodexHarness::new().with_executable(wrapper);
    let (mut controls, _steer, _token) = controls("Yes");
    controls.mcp_servers = vec![common::board_mcp_server()];

    run_to_end(&harness, request("scenario:happy"), controls).await;

    let argv = std::fs::read_to_string(argv_file).expect("spawned Codex argv");
    let argv: Vec<&str> = argv.lines().collect();
    assert_eq!(
        argv,
        [
            "-c",
            r#"mcp_servers."comet-board"={ command = "comet-board", args = ["mcp"] }"#,
            "app-server"
        ]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn spawned_codex_forces_push_credentials_through_its_shell_policy() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (wrapper, argv_file) = common::recording_wrapper(&fixture_path(), &dir);
    let harness = CodexHarness::new().with_executable(wrapper);
    let (mut controls, _steer, _token) = controls("Yes");
    controls.chat_id = Some("chat-464".into());
    controls.push = Some(PushCredentials {
        env: vec![
            ("GIT_ASKPASS".into(), "/board/bin/comet-askpass".into()),
            ("COMET_BOARD_ASKPASS_REPO".into(), "owner/repo".into()),
        ],
        bin_dir: Some(PathBuf::from("/board/bin")),
    });

    run_to_end(&harness, request("scenario:happy"), controls).await;

    let argv = std::fs::read_to_string(argv_file).expect("spawned Codex argv");
    let argv: Vec<&str> = argv.lines().collect();
    assert_eq!(argv.first(), Some(&"-c"), "{argv:?}");
    let policy = argv.get(1).expect("credential policy override");
    assert!(
        policy.starts_with("shell_environment_policy={ inherit = \"core\""),
        "{policy}"
    );
    assert!(!policy.contains("include_only"), "{policy}");
    assert!(
        policy.contains(r#"filters = { "*" = "include" }"#),
        "{policy}"
    );
    assert!(
        policy.contains(r#""GIT_ASKPASS" = "/board/bin/comet-askpass""#),
        "{policy}"
    );
    assert!(
        policy.contains(r#""COMET_BOARD_CHAT_ID" = "chat-464""#),
        "{policy}"
    );
    assert!(
        policy.contains(r#""PATH" = "/board/bin:"#),
        "the gh shim must lead Codex's tool PATH: {policy}"
    );
    assert_eq!(
        argv.get(2..6),
        Some(&["-c", "allow_login_shell=false", "-c", "features.shell_snapshot=false"][..]),
        "credentialed runs must prohibit profile mutation after policy application: {argv:?}"
    );
    assert_eq!(argv.last(), Some(&"app-server"), "{argv:?}");
}

/// The #464 boundary in one process tree: the harness emits its override, a
/// real Codex app server merges it over a restrictive account config and runs
/// a real `git push` through the thread's configured shell in a subdirectory.
/// Hostile noninteractive startup hooks must not load, and git must choose
/// askpass over an available ambient helper. Both bash's lower-policy BASH_ENV
/// and zsh's always-read .zshenv cross the real model-tool boundary.
/// The stand-in board writes the same ledger event as `git-askpass`, so the
/// final assertion is the settlement input rather than a marker invented by
/// the test.
#[cfg(unix)]
#[tokio::test]
async fn restrictive_codex_tool_push_is_attested_to_the_dispatch_chat() {
    if !std::process::Command::new("codex")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        assert_ne!(
            std::env::var_os("COMET_REQUIRE_REAL_CODEX"),
            Some("1".into()),
            "CI requires the pinned real Codex CLI for the push-policy regression"
        );
        eprintln!(
            "no real Codex CLI on this developer box — skipping subprocess-policy regression"
        );
        return;
    }
    for shell_name in ["bash", "zsh"] {
        let shell = std::env::var_os("PATH")
            .into_iter()
            .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .map(|dir| dir.join(shell_name))
            .find(|candidate| candidate.is_file());
        let Some(shell) = shell else {
            assert_ne!(
                std::env::var_os("COMET_REQUIRE_REAL_CODEX"),
                Some("1".into()),
                "CI requires {shell_name} for the startup-hook regression"
            );
            eprintln!("no {shell_name} on this developer box — skipping that startup hook");
            continue;
        };
        restrictive_codex_tool_push_case(shell_name, &shell).await;
    }
}

#[cfg(unix)]
async fn restrictive_codex_tool_push_case(shell_name: &str, shell: &std::path::Path) {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = dir.path().join("state");
    let paths = Paths {
        config_dir: dir.path().join("config"),
        state_dir: state.clone(),
    };
    std::fs::create_dir_all(&state).expect("state dir");

    let board = dir.path().join("board-helper");
    let ledger = credential_ledger::path(&paths);
    std::fs::write(
        &board,
        format!(
            "#!/bin/sh\n\
             [ \"$1\" = git-askpass ] || exit 2\n\
             case \"$2\" in\n\
               *sername*) echo x-access-token ;;\n\
               *) printf '{{\"at\":\"test\",\"event\":\"minted\",\"tool\":\"git-askpass\",\"repo\":\"owner/repo\",\"chat\":\"%s\"}}\\n' \"$COMET_BOARD_CHAT_ID\" >> '{}'; echo board-token ;;\n\
             esac\n",
            ledger.display()
        ),
    )
    .expect("fake board");
    std::fs::set_permissions(&board, std::fs::Permissions::from_mode(0o755)).expect("chmod board");
    let askpass = git_credentials::install_askpass_shim(&dir.path().join("bin"), &board)
        .expect("askpass shim");

    // A credential the push could use if the board's empty helper override is
    // filtered out. The server records which password actually reaches it.
    let ambient = dir.path().join("ambient-helper");
    let ambient_used = dir.path().join("ambient-used");
    std::fs::write(
        &ambient,
        format!(
            "#!/bin/sh\nprintf used > '{}'\n[ \"$1\" = get ] && printf 'username=x-access-token\\npassword=ambient-token\\n'\n",
            ambient_used.display()
        ),
    )
    .expect("ambient helper");
    std::fs::set_permissions(&ambient, std::fs::Permissions::from_mode(0o755))
        .expect("chmod ambient helper");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::write(
        home.join(".gitconfig"),
        format!("[credential]\n\thelper = {}\n", ambient.display()),
    )
    .expect("ambient git config");
    let hostile_profile = dir.path().join(format!("{shell_name}-startup-loaded"));
    let profile = format!(
        "printf loaded > '{}'\n\
         unset GIT_ASKPASS COMET_BOARD_CHAT_ID COMET_BOARD_CREDENTIAL_LEDGER\n\
         export PATH=/usr/bin:/bin\n",
        hostile_profile.display()
    );
    let bash_env = home.join("bash-env");
    std::fs::write(&bash_env, &profile).expect("hostile BASH_ENV");
    std::fs::write(home.join(".zshenv"), &profile).expect("hostile .zshenv");

    let tool_cwd = dir.path().join("source");
    std::fs::create_dir_all(&tool_cwd).unwrap();
    for args in [
        vec!["init", "-b", "main"],
        vec!["config", "user.email", "codex@test.invalid"],
        vec!["config", "user.name", "Codex Test"],
        vec!["commit", "--allow-empty", "-m", "non-shallow source"],
    ] {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&tool_cwd)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    let model_listener = TcpListener::bind("127.0.0.1:0").expect("model listener");
    let model_base_url = format!("http://{}/v1", model_listener.local_addr().unwrap());
    let tool_arguments = serde_json::json!({
        // Codex chooses the account's passwd shell. Invoke the target shell
        // inside that real model-tool command so one CI host exercises both
        // noninteractive startup mechanisms with the policy-built env.
        "cmd": format!(
            "{} -c 'git push \"$COMET_TEST_PUSH_URL\" HEAD:refs/heads/board-test'",
            shell.display()
        ),
        "workdir": tool_cwd.clone(),
        "yield_time_ms": 10_000
    })
    .to_string();
    let first_response = format!(
        "event: response.created\ndata: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp-1\"}}}}\n\n\
         event: response.output_item.done\ndata: {{\"type\":\"response.output_item.done\",\"item\":{{\"type\":\"function_call\",\"call_id\":\"push-464\",\"name\":\"exec_command\",\"arguments\":{}}}}}\n\n\
         event: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp-1\",\"usage\":{{\"input_tokens\":0,\"output_tokens\":0,\"total_tokens\":0}}}}}}\n\n",
        serde_json::to_string(&tool_arguments).expect("tool arguments encode")
    );
    let second_response = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-2\"}}\n\n\
         event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"id\":\"msg-1\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}}\n\n\
         event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-2\",\"usage\":{\"input_tokens\":0,\"output_tokens\":0,\"total_tokens\":0}}}\n\n"
        .to_string();
    let model_requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let recorded_model_requests = Arc::clone(&model_requests);
    std::thread::spawn(move || {
        for (index, stream) in model_listener.incoming().take(2).enumerate() {
            let mut stream = stream.expect("model request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone model request"));
            let mut content_length = 0;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = value.trim().parse().expect("content length");
                }
            }
            let mut body = vec![0; content_length];
            reader.read_exact(&mut body).expect("model request body");
            recorded_model_requests
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&body).into_owned());
            let response = if index % 2 == 0 {
                &first_response
            } else {
                &second_response
            };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            )
            .expect("model response");
        }
    });

    let origin = dir.path().join("origin.git");
    std::process::Command::new("git").args(["init", "--bare", origin.to_str().unwrap()]).output().unwrap();
    let configured = std::process::Command::new("git")
        .args(["--git-dir", origin.to_str().unwrap(), "config", "http.receivepack", "true"])
        .output().unwrap();
    assert!(configured.status.success());
    let url = authenticated_git_http(origin.clone());

    // Capture the exact overlay the harness will pass, then give it to the
    // real Codex app server over an adversarial lower account policy. This is
    // Codex's own recursive config merge and model-issued `exec_command`, not
    // a local model of either boundary.
    let (wrapper, argv_file) = common::recording_wrapper(&fixture_path(), &dir);
    let harness = CodexHarness::new().with_executable(wrapper);
    let (mut controls, _steer, _token) = controls("Yes");
    controls.chat_id = Some("chat-464".into());
    credential_ledger::handed(&paths, "owner/repo", Some("chat-464"));
    let mut env = git_credentials::push_env(&askpass, "owner/repo");
    env.push(("COMET_TEST_PUSH_URL".into(), url.clone()));
    let real_git = git_credentials::resolve_git(None).expect("real git");
    let guarded_bin = git_credentials::install_git_shim(
        &dir.path().join("guarded-bin"),
        &real_git,
        &askpass,
        "owner/repo",
        &paths,
        comet_proto::GithubPushContract { contents_write: true, workflows_write: true },
        "chat-464",
    ).expect("git guard");
    controls.push = Some(PushCredentials { env, bin_dir: Some(guarded_bin.clone()) });
    run_to_end(&harness, request("scenario:happy"), controls).await;
    let argv = std::fs::read_to_string(argv_file).expect("Codex argv");
    let argv: Vec<&str> = argv.lines().collect();
    let policy = argv.get(1).expect("policy overlay");
    assert_eq!(argv.get(3), Some(&"allow_login_shell=false"), "{argv:?}");
    assert_eq!(argv.get(5), Some(&"features.shell_snapshot=false"), "{argv:?}");
    assert!(policy.contains(r#""BASH_ENV" = """#), "{policy}");
    assert!(policy.contains(r#""ENV" = """#), "{policy}");
    assert!(policy.contains(r#""ZDOTDIR" = "/dev/null""#), "{policy}");

    let codex_home = dir.path().join("codex-home");
    std::fs::create_dir_all(&codex_home).expect("Codex home");
    std::fs::write(
        codex_home.join("config.toml"),
        format!(
            "model = \"gpt-5.4\"\nmodel_provider = \"mock\"\n\
             [model_providers.mock]\nname = \"mock\"\nbase_url = \"{model_base_url}\"\nwire_api = \"responses\"\nrequires_openai_auth = false\nrequest_max_retries = 0\nstream_max_retries = 0\n\
             [features]\nunified_exec = true\nshell_snapshot = true\n\
             [shell_environment_policy]\ninherit = \"none\"\ninclude_only = [\"HOME\"]\n\
             [shell_environment_policy.set]\nBASH_ENV = {}\nENV = {}\nZDOTDIR = {}\n",
            serde_json::to_string(&bash_env.display().to_string()).expect("BASH_ENV encodes"),
            serde_json::to_string(&bash_env.display().to_string()).expect("ENV encodes"),
            serde_json::to_string(&home.display().to_string()).expect("ZDOTDIR encodes")
        ),
    )
    .expect("restrictive lower config");
    // Now traverse the production harness itself, not a reconstructed argv.
    // The wrapper supplies only the isolated account directories; every
    // policy flag, environment projection, and app-server request comes from
    // `CodexHarness::run`.
    let real_codex = std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
        .map(|dir| dir.join("codex"))
        .find(|path| path.is_file())
        .expect("Codex on PATH");
    let live = dir.path().join("live-codex");
    std::fs::write(
        &live,
        format!(
            "#!/bin/sh\nexport CODEX_HOME='{}' HOME='{}'\nexec '{}' \"$@\"\n",
            codex_home.display(),
            home.display(),
            real_codex.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&live, std::fs::Permissions::from_mode(0o755)).unwrap();
    let live_harness = CodexHarness::new().with_executable(live);
    let (mut live_controls, _, _) = self::controls("Yes");
    live_controls.chat_id = Some("chat-464".into());
    let mut live_env = git_credentials::push_env(&askpass, "owner/repo");
    live_env.push(("COMET_TEST_PUSH_URL".into(), url));
    live_controls.push = Some(PushCredentials {
        env: live_env,
        bin_dir: Some(guarded_bin),
    });
    let mut live_request = request("push the current branch");
    live_request.cwd = tool_cwd.display().to_string();
    // This test itself runs inside macOS seatbelt; nesting another workspace
    // seatbelt around a temp checkout is refused before Git starts. Credential
    // enforcement is the boundary under test, so let the inner Codex run use
    // the same unsandboxed tool mode the former direct app-server fixture did.
    live_request.sandbox = SandboxLevel::DangerFullAccess;
    let before = std::process::Command::new("git")
        .args(["--git-dir", origin.to_str().unwrap(), "show-ref", "--verify", "--quiet", "refs/heads/board-test"])
        .status().unwrap();
    assert!(!before.success(), "production ref existed before the harness push");
    let events = run_to_end(&live_harness, live_request, live_controls).await;
    assert!(
        !events.iter().any(|event| matches!(event, AgentEvent::Error { .. })),
        "production harness emitted an error: {events:?}"
    );

    let pushed = std::process::Command::new("git").args(["--git-dir", origin.to_str().unwrap(), "rev-parse", "refs/heads/board-test"]).output().unwrap();
    assert!(pushed.status.success(), "origin did not change; events: {events:?}; model requests: {:?}", model_requests.lock().unwrap());
    assert!(
        !hostile_profile.exists(),
        "Codex sourced the hostile {shell_name} startup hook after constructing the protected environment"
    );
    let record = credential_ledger::for_chat(&paths, "chat-464");
    assert!(record.handed, "the attempt did not record its handoff");
    assert!(record.minted, "the push was not attributed to its dispatch");
    assert!(
        !record.unsanctioned(),
        "an attested push settled as alternate"
    );
    assert!(
        comet_board::sync::credential_is_sanctioned_at_settle(&paths, "chat-464"),
        "the production settlement verdict rejected the model-issued push"
    );
    assert!(
        !ambient_used.exists(),
        "Git selected the hostile ambient credential helper"
    );
}

#[tokio::test]
async fn steering_uses_turn_steer_with_expected_turn_id() {
    let (controls, steer, _token) = controls("Yes");
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
        .expect("Steered emitted: {events:?}");
    assert!(steered.0.is_some() && steered.1.is_some());
    assert_ne!(steered.0, steered.1);

    // The fake only emits this delta after verifying expectedTurnId + text.
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "steered".into()
    }));
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("th-1".into()),
        })
    );
}

#[tokio::test]
async fn rejected_steer_falls_back_to_a_follow_up_turn() {
    let (controls, steer, _token) = controls("Yes");
    steer
        .send(SteerMessage {
            prompt: "redirect please".into(),
            message_id: None,
        })
        .await
        .expect("steer queued");
    let events = run_to_end(&harness(), request("scenario:steer-race"), controls).await;

    // Two turns: the raced one completes, then the fallback carries the steer.
    let dones: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Done { status, .. } => Some(*status),
            _ => None,
        })
        .collect();
    assert_eq!(
        dones,
        vec![DoneStatus::Completed, DoneStatus::Completed],
        "{events:?}"
    );
    let steered_pos = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Steered { .. }))
        .expect("Steered emitted on fallback");
    let first_done_pos = events
        .iter()
        .position(|e| matches!(e, AgentEvent::Done { .. }))
        .expect("first done");
    assert!(
        first_done_pos < steered_pos,
        "fallback turn starts after the raced turn ends: {events:?}"
    );
    // Only emitted by the fake when the fallback turn/start carried the text.
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "fallback".into()
    }));
}

#[tokio::test]
async fn approvals_round_trip_as_input_requests() {
    // Approvals must reach the ENGINE's input bridge (`request_input`) — and
    // the harness must NOT emit its own `InputRequested`/`InputResolved`
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
                    labels: vec!["Yes".into()],
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
    let mut req = request("scenario:approve");
    req.auto_approve = false;
    let events = run_to_end(&harness(), req, controls).await;

    let asked = asked.lock().unwrap();
    assert_eq!(asked.len(), 2, "{events:?}");
    assert_eq!(asked[0].header, "Approve command");
    assert!(asked[0].question.contains("rm -rf /tmp/x"));
    assert_eq!(asked[0].options, vec!["Yes".to_string(), "No".to_string()]);
    assert_eq!(asked[1].header, "Approve file change");
    assert!(asked[1].question.contains("/tmp/a.rs"));
    assert!(
        !events.iter().any(|e| matches!(
            e,
            AgentEvent::InputRequested { .. } | AgentEvent::InputResolved { .. }
        )),
        "harness must not emit input lifecycle events itself: {events:?}"
    );

    // The fake only completes the turn after seeing BOTH accept decisions.
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("th-1".into()),
        })
    );
}

#[tokio::test]
async fn approval_no_answer_becomes_decline() {
    let (controls, _steer, _token) = controls("No");
    let mut req = request("scenario:decline");
    req.auto_approve = false;
    let events = run_to_end(&harness(), req, controls).await;

    // The fake only completes the turn after seeing the decline decision.
    assert!(
        matches!(
            events.last(),
            Some(AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            })
        ),
        "{events:?}"
    );
}

#[tokio::test]
async fn interrupt_sends_turn_interrupt_and_maps_aborted() {
    let (controls, _steer, token) = controls("Yes");
    let mut stream = harness()
        .run(request("scenario:interrupt"), controls)
        .await
        .expect("run starts");

    let mut events = Vec::new();
    let drain = async {
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(&ev, AgentEvent::TextDelta { text } if text == "working") {
                token.cancel(); // interrupt mid-turn
            }
            events.push(ev);
        }
    };
    let finished = tokio::time::timeout(common::scaled(RUN_DEADLINE), drain).await;
    assert!(
        finished.is_ok(),
        "the interrupted run did not end within {}\n  events before the deadline ({}):\n{}",
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
            session_id: Some("th-1".into()),
        })
    );
}

#[tokio::test]
async fn unresponsive_child_is_reaped_with_interrupted_done() {
    let harness = CodexHarness::new()
        .with_executable(fixture_path())
        .with_graces(Duration::from_millis(100), Duration::from_millis(500));
    let (controls, _steer, token) = controls("Yes");
    let mut stream = harness
        .run(request("scenario:wedge"), controls)
        .await
        .expect("run starts");

    let mut events = Vec::new();
    let drain = async {
        while let Some(ev) = stream.next().await {
            let ev = ev.expect("stream event");
            if matches!(&ev, AgentEvent::TextDelta { text } if text == "working") {
                token.cancel();
            }
            events.push(ev);
        }
    };
    let finished = tokio::time::timeout(common::scaled(RUN_DEADLINE), drain).await;
    assert!(
        finished.is_ok(),
        "the wedged child was not escalated and reaped within {} — \
         the SIGKILL escalation never landed\n  events before the deadline ({}):\n{}",
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
            session_id: Some("th-1".into()),
        })
    );
}

#[tokio::test]
async fn turn_failed_maps_to_errored_done() {
    let (controls, _steer, _token) = controls("Yes");
    let events = run_to_end(&harness(), request("scenario:fail"), controls).await;
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Errored,
            result: None,
            error: Some("boom".into()),
            session_id: Some("th-1".into()),
        })
    );
}

/// §gh#394. The sandbox failed to come up, so no command in the run can start
/// — not even `pwd`. The agent is alive and can still talk, which is exactly
/// the failure mode: it cannot push, cannot open a pull request, cannot claim,
/// and cannot signal blocked either, so the board row reads `working` until a
/// human notices the prose. The run has to end, and end as errored, which is
/// what the board reads as blocked and retryable.
#[tokio::test]
async fn a_sandbox_that_cannot_start_a_command_ends_the_run_instead_of_hanging() {
    let (controls, _steer, _token) = controls("Yes");
    let events = run_to_end(&harness(), request("scenario:sandbox-dead"), controls).await;

    let Some(AgentEvent::Done {
        status,
        error: Some(error),
        session_id,
        ..
    }) = events.last()
    else {
        panic!("the run must end with an errored Done: {events:?}");
    };
    assert_eq!(*status, DoneStatus::Errored);
    assert_eq!(session_id.as_deref(), Some("th-1"));
    assert!(
        error.contains("sandbox") && error.contains("Can't mkdir parents"),
        "the reason must name the sandbox and quote the line: {error}"
    );

    // The failed command is still on the record as a command — a reviewer sees
    // what was attempted, not only that the run died.
    assert!(
        events.contains(&AgentEvent::ToolResult {
            id: "c1".into(),
            is_error: true,
        }),
        "{events:?}"
    );
    // And the same reason is visible in the transcript, not only in the Done.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Error { message } if message.contains("sandbox"))),
        "{events:?}"
    );
}

#[tokio::test]
async fn resume_falls_back_to_fresh_thread() {
    let (controls, _steer, _token) = controls("Yes");
    let mut req = request("scenario:resumed");
    req.resume = Some("resume-fail".into());
    let events = run_to_end(&harness(), req, controls).await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::SessionStarted { session_id, .. } if session_id == "th-fresh"
        )),
        "fresh thread expected: {events:?}"
    );
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("th-fresh".into()),
        })
    );
}

#[tokio::test]
async fn resume_reuses_the_existing_thread() {
    let (controls, _steer, _token) = controls("Yes");
    let mut req = request("scenario:resumed");
    req.resume = Some("resume-ok".into());
    let events = run_to_end(&harness(), req, controls).await;

    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::SessionStarted { session_id, .. } if session_id == "th-resumed"
        )),
        "resumed thread expected: {events:?}"
    );
}

#[tokio::test]
async fn missing_binary_is_not_installed() {
    let harness = CodexHarness::new().with_executable("/nonexistent/codex-nowhere");
    let (controls, _steer, _token) = controls("Yes");
    let err = harness
        .run(request("scenario:happy"), controls)
        .await
        .err()
        .expect("spawn fails");
    assert!(matches!(err, HarnessError::NotInstalled(_)), "{err:?}");
}

#[tokio::test]
async fn models_returns_curated_catalog() {
    let models = harness().models().await.expect("models");
    assert_eq!(models.len(), 7);
    assert_eq!(models[0].id, "gpt-5.6-sol");
    assert!(models[0].reasoning_levels.contains(&ReasoningLevel::Ultra));
    assert!(
        models
            .iter()
            .all(|m| m.options.iter().any(|o| o.id == "serviceTier"))
    );

    let missing = CodexHarness::new().with_executable("/nonexistent/codex-nowhere");
    // models() requires a resolvable binary… but with_executable trusts the
    // caller's path, so only the default resolution can report NotInstalled —
    // exercise the harness identity surface instead.
    assert_eq!(missing.id(), HarnessId::Codex);
    // "Codex" — comet composer/defaults.ts HARNESS_LABEL (and the registry's
    // lazy descriptor must stay in lockstep).
    assert_eq!(missing.display_name(), "Codex");
    assert_eq!(missing.reasoning_levels().len(), 7);
}

/// §gh#349. Codex's `workspace-write` denies writes to exactly `<cwd>/.git`, so
/// a dispatched agent in a main checkout can edit files and cannot commit them.
/// The fix is a permission grant, not a sandbox drop: the checkout's git
/// metadata rides `sandboxPolicy.writableRoots`, which the fixture asserts on
/// the wire — and the run announces the level it actually got before it says
/// anything else.
#[tokio::test]
async fn a_checkouts_git_dir_rides_the_wire_and_the_level_is_announced() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let checkout = tmp.path().join("repo");
    std::fs::create_dir_all(checkout.join(".git")).expect("fake checkout");

    let (controls, _steer, _token) = controls("Yes");
    let mut req = request("scenario:sandbox-roots");
    req.cwd = checkout.display().to_string();
    let events = run_to_end(&harness(), req, controls).await;

    // The fixture fails the turn when `writableRoots` is missing, so reaching
    // a completed Done IS the wire assertion.
    assert!(
        events.contains(&AgentEvent::Done {
            status: DoneStatus::Completed,
            result: None,
            error: None,
            session_id: Some("th-1".into()),
        }),
        "the git dir did not reach sandboxPolicy.writableRoots: {events:?}"
    );

    // And the run said what it got, first, and before SessionStarted — a
    // journal's first line about a run is the terms it ran on.
    let reports: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Sandbox(report) => Some(report),
            _ => None,
        })
        .collect();
    assert_eq!(reports.len(), 1, "one per run: {events:?}");
    let report = reports[0];
    // A modern codex on a plain checkout keeps its sandbox: nothing widened,
    // and so nothing for a reviewer to be told.
    assert_eq!(report.requested, SandboxLevel::WorkspaceWrite);
    assert_eq!(report.effective, SandboxLevel::WorkspaceWrite);
    assert!(!report.widened_beyond_request());
    assert_eq!(report.note(), None);
    assert!(matches!(events[0], AgentEvent::Sandbox(_)), "{events:?}");
}
