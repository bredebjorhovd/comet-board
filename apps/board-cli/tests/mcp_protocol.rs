use std::process::Stdio;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use comet_rpc::{RpcError, RpcReply, RpcService, methods, serve_ws_listener};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

#[derive(Clone)]
enum DispatchReply {
    Success,
    RequireOwn,
}

#[derive(Clone)]
struct FakeBoard {
    rows: Value,
    accounts: Value,
    accounts_error: Option<String>,
    dispatch_reply: DispatchReply,
    dispatches: Arc<Mutex<Vec<Value>>>,
    auth_status_calls: Arc<Mutex<usize>>,
}

impl FakeBoard {
    fn new(rows: Vec<Value>, accounts: Value, dispatch_reply: DispatchReply) -> Self {
        Self {
            rows: Value::Array(rows),
            accounts,
            accounts_error: None,
            dispatch_reply,
            dispatches: Arc::new(Mutex::new(Vec::new())),
            auth_status_calls: Arc::new(Mutex::new(0)),
        }
    }

    fn with_accounts_error(mut self, message: &str) -> Self {
        self.accounts_error = Some(message.to_string());
        self
    }
}

#[async_trait]
impl RpcService for FakeBoard {
    async fn handle(&self, method: &str, params: Value) -> Result<RpcReply, RpcError> {
        match method {
            methods::WATCH_BOARD => Ok(RpcReply::Stream(
                futures::stream::iter([self.rows.clone()]).boxed(),
            )),
            methods::LIST_AGENT_ACCOUNTS => match &self.accounts_error {
                Some(message) => Err(RpcError::Failed(message.clone())),
                None => Ok(RpcReply::Value(self.accounts.clone())),
            },
            methods::DISPATCH_TASK => {
                self.dispatches.lock().unwrap().push(params);
                match self.dispatch_reply {
                    DispatchReply::Success => Ok(RpcReply::Value(json!({
                        "chatId": "child-chat",
                        "cwd": "/tmp/child",
                        "attempt": 1
                    }))),
                    DispatchReply::RequireOwn => Err(RpcError::Failed(
                        "billing_guard = \"require-own\": this run bills somebody else".into(),
                    )),
                }
            }
            methods::AUTH_STATUS => {
                *self.auth_status_calls.lock().unwrap() += 1;
                Err(RpcError::Failed(
                    "the MCP server must not borrow the host user's identity".into(),
                ))
            }
            other => Err(RpcError::UnknownMethod(other.into())),
        }
    }
}

struct McpSession {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
}

impl McpSession {
    async fn start(service: FakeBoard, chat_id: &str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(serve_ws_listener(listener, Arc::new(service)));

        let mut child = Command::new(env!("CARGO_BIN_EXE_comet-board"))
            .arg("--port")
            .arg(port.to_string())
            .arg("mcp")
            .env("COMET_BOARD_CHAT_ID", chat_id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        Self {
            child,
            stdin,
            stdout,
        }
    }

    async fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        self.stdin
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let line = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.stdout.next_line(),
        )
        .await
        .expect("MCP response timed out")
        .expect("reading MCP response")
        .expect("MCP server exited before responding");
        serde_json::from_str(&line).unwrap()
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

fn row(
    id: &str,
    identifier: &str,
    chat_id: Option<&str>,
    account: Option<&str>,
    billed_to: Option<&str>,
) -> Value {
    json!({
        "id": id,
        "identifier": identifier,
        "title": "test task",
        "state": if chat_id.is_some() { "working" } else { "ready" },
        "source": "github",
        "url": "https://example.test/task",
        "labels": [],
        "dispatchable": true,
        "gone": false,
        "route": "test",
        "workspace": "test",
        "runtime": "claude",
        "chat_id": chat_id,
        "pr_url": null,
        "pr_number": null,
        "branch": null,
        "dispatched_by": null,
        "dispatched_by_chat": null,
        "last_outcome": null,
        "last_outcome_at": null,
        "attempts": if chat_id.is_some() { 1 } else { 0 },
        "reopened": 0,
        "account": account,
        "billed_to": billed_to
    })
}

fn accounts(values: &[(&str, &str, bool)]) -> Value {
    json!({
        "accounts": values
            .iter()
            .map(|(id, email, active)| json!({
                "id": id,
                "harness": "claude-code",
                "email": email,
                "planLabel": null,
                "active": active,
                "usageWindows": [],
                "switchable": true
            }))
            .collect::<Vec<_>>(),
        "warnings": []
    })
}

fn tool_call(name: &str, arguments: Value) -> Value {
    json!({ "name": name, "arguments": arguments })
}

#[tokio::test]
async fn initialize_and_dispatch_keep_agent_provenance_without_host_identity() {
    let service = FakeBoard::new(
        vec![
            row(
                "gh:owner/repo#1",
                "gh#1",
                Some("parent-chat"),
                Some("alice"),
                Some("alice@example.com"),
            ),
            row(
                "gh:owner/repo#2",
                "gh#2",
                None,
                Some("alice"),
                None,
            ),
        ],
        accounts(&[("alice", "alice@example.com", true)]),
        DispatchReply::Success,
    );
    let observed = service.clone();
    let mut mcp = McpSession::start(service, "parent-chat").await;

    let initialized = mcp
        .request(
            1,
            "initialize",
            json!({ "protocolVersion": "2025-06-18" }),
        )
        .await;
    assert_eq!(initialized["result"]["serverInfo"]["name"], "comet-board");

    let response = mcp
        .request(
            2,
            "tools/call",
            tool_call("dispatch_task", json!({ "task": "gh:owner/repo#2" })),
        )
        .await;
    assert_eq!(response["result"]["isError"], false, "{response:#}");

    let dispatches = observed.dispatches.lock().unwrap();
    assert_eq!(dispatches.len(), 1);
    assert_eq!(dispatches[0]["via"], "parent-chat");
    assert!(dispatches[0].get("viaUser").is_none(), "{:#}", dispatches[0]);
    assert!(dispatches[0].get("bill").is_none(), "{:#}", dispatches[0]);
    assert_eq!(*observed.auth_status_calls.lock().unwrap(), 0);
}

#[tokio::test]
async fn warn_mode_cross_billing_is_refused_before_dispatch() {
    let service = FakeBoard::new(
        vec![
            row(
                "gh:owner/repo#1",
                "gh#1",
                Some("parent-chat"),
                Some("alice"),
                Some("alice@example.com"),
            ),
            row(
                "gh:owner/repo#2",
                "gh#2",
                None,
                Some("bob"),
                None,
            ),
        ],
        accounts(&[
            ("alice", "alice@example.com", true),
            ("bob", "bob@example.com", false),
        ]),
        // A warn-mode board would accept this call. The MCP boundary must stop
        // it before that mutation because an agent cannot acknowledge a payer.
        DispatchReply::Success,
    );
    let observed = service.clone();
    let mut mcp = McpSession::start(service, "parent-chat").await;

    let response = mcp
        .request(
            1,
            "tools/call",
            tool_call("dispatch_task", json!({ "task": "gh:owner/repo#2" })),
        )
        .await;
    assert_eq!(response["result"]["isError"], true, "{response:#}");
    let error = response["result"]["structuredContent"]["error"]
        .as_str()
        .unwrap();
    assert!(error.contains("bob@example.com"), "{error}");
    assert!(error.contains("human"), "{error}");
    assert!(observed.dispatches.lock().unwrap().is_empty());
}

#[tokio::test]
async fn dispatch_is_refused_when_the_parent_attempt_has_no_recorded_payer() {
    let service = FakeBoard::new(
        vec![
            row(
                "gh:owner/repo#1",
                "gh#1",
                Some("parent-chat"),
                Some("alice"),
                None,
            ),
            row(
                "gh:owner/repo#2",
                "gh#2",
                None,
                Some("alice"),
                None,
            ),
        ],
        accounts(&[("alice", "alice@example.com", true)]),
        DispatchReply::Success,
    );
    let observed = service.clone();
    let mut mcp = McpSession::start(service, "parent-chat").await;

    let response = mcp
        .request(
            1,
            "tools/call",
            tool_call("dispatch_task", json!({ "task": "gh:owner/repo#2" })),
        )
        .await;

    assert_eq!(response["result"]["isError"], true, "{response:#}");
    let error = response["result"]["structuredContent"]["error"]
        .as_str()
        .unwrap();
    assert!(error.contains("recorded payer"), "{error}");
    assert!(error.contains("human"), "{error}");
    assert!(observed.dispatches.lock().unwrap().is_empty());
}

#[tokio::test]
async fn dispatch_is_refused_when_the_account_list_cannot_be_read() {
    let service = FakeBoard::new(
        vec![
            row(
                "gh:owner/repo#1",
                "gh#1",
                Some("parent-chat"),
                Some("alice"),
                Some("alice@example.com"),
            ),
            row(
                "gh:owner/repo#2",
                "gh#2",
                None,
                Some("alice"),
                None,
            ),
        ],
        accounts(&[("alice", "alice@example.com", true)]),
        DispatchReply::Success,
    )
    .with_accounts_error("account catalog unavailable");
    let observed = service.clone();
    let mut mcp = McpSession::start(service, "parent-chat").await;

    let response = mcp
        .request(
            1,
            "tools/call",
            tool_call("dispatch_task", json!({ "task": "gh:owner/repo#2" })),
        )
        .await;

    assert_eq!(response["result"]["isError"], true, "{response:#}");
    let error = response["result"]["structuredContent"]["error"]
        .as_str()
        .unwrap();
    assert!(error.contains("agent accounts"), "{error}");
    assert!(error.contains("human"), "{error}");
    assert!(observed.dispatches.lock().unwrap().is_empty());
}

#[tokio::test]
async fn require_own_cross_billing_is_preflighted_and_bill_cannot_be_forged() {
    let service = FakeBoard::new(
        vec![
            row(
                "gh:owner/repo#1",
                "gh#1",
                Some("parent-chat"),
                Some("alice"),
                Some("alice@example.com"),
            ),
            row(
                "gh:owner/repo#2",
                "gh#2",
                None,
                Some("bob"),
                None,
            ),
        ],
        accounts(&[
            ("alice", "alice@example.com", true),
            ("bob", "bob@example.com", false),
        ]),
        DispatchReply::RequireOwn,
    );
    let observed = service.clone();
    let mut mcp = McpSession::start(service, "parent-chat").await;

    let refused = mcp
        .request(
            1,
            "tools/call",
            tool_call("dispatch_task", json!({ "task": "gh:owner/repo#2" })),
        )
        .await;
    assert_eq!(refused["result"]["isError"], true, "{refused:#}");
    assert!(refused["result"]["structuredContent"]["error"]
        .as_str()
        .unwrap()
        .contains("bob@example.com"));
    assert!(observed.dispatches.lock().unwrap().is_empty());

    let forged = mcp
        .request(
            2,
            "tools/call",
            tool_call(
                "dispatch_task",
                json!({
                    "task": "gh:owner/repo#2",
                    "bill": "bob@example.com"
                }),
            ),
        )
        .await;
    assert_eq!(forged["result"]["isError"], true, "{forged:#}");
    assert!(forged["result"]["structuredContent"]["error"]
        .as_str()
        .unwrap()
        .contains("unknown field `bill`"));
    assert!(observed.dispatches.lock().unwrap().is_empty());
}

#[tokio::test]
async fn duplicate_short_task_ids_are_ambiguous() {
    let service = FakeBoard::new(
        vec![
            row("gh:owner/one#14", "gh#14", None, None, None),
            row("gh:owner/two#14", "gh#14", None, None, None),
        ],
        accounts(&[]),
        DispatchReply::Success,
    );
    let mut mcp = McpSession::start(service, "unrelated-chat").await;

    let response = mcp
        .request(
            1,
            "tools/call",
            tool_call("task_status", json!({ "task": "gh#14" })),
        )
        .await;
    assert_eq!(response["result"]["isError"], true, "{response:#}");
    let error = response["result"]["structuredContent"]["error"]
        .as_str()
        .unwrap();
    assert!(error.contains("gh:owner/one#14"), "{error}");
    assert!(error.contains("gh:owner/two#14"), "{error}");
}
