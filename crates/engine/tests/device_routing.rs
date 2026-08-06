//! M4b integration: `targetDeviceId` routing — engine A forwards device-addressed RPCs
//! to engine B through B's device-room relay (host relay on B, link cache on A), with a
//! minimal in-memory device-room standing in for the edge DO (route client→host with
//! `from` stamped, host→client by `to`).
//!
//! The stand-in also enforces the DO's authorization rule (`edge/src/device-room.ts`,
//! gh#66): the host claims the room for its user AND org, the owner reaches it from any
//! device, any other member of that org reaches it as a client, and everyone else is
//! refused at the handshake. That gate is the whole reason a teammate can drive the
//! box's board, so the routing tests run with it in place rather than against an open
//! relay. Dev-mode bearers carry identity as `user@org`, exactly like the edge's.

// tungstenite's `accept_hdr_async` callback signature fixes the Err type as a full
// `Response` — its size is not ours to shrink.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse as WsErrorResponse, Request as WsRequest, Response as WsResponse,
};

use comet_doc::SessionCommandPayload;
use comet_engine::{EngineCore, HarnessRegistry};
use comet_harness::{Harness, HarnessError, RunControls};
use comet_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel,
    SteeringMode,
};
use comet_rpc::{
    DeviceFrameHeader, LinkCache, LinkCacheConfig, StaticToken, decode_device_frame,
    encode_device_frame, methods,
};

// ---------------------------------------------------------------------------
// Minimal in-memory device room (route-only subset of the DO semantics)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RelayState {
    host: Option<mpsc::UnboundedSender<Vec<u8>>>,
    clients: HashMap<String, mpsc::UnboundedSender<Vec<u8>>>,
    /// Claim-on-first-host: `(userId, orgId)`, the identity anchor every later
    /// join is checked against.
    owner: Option<(String, String)>,
}

/// A dev-mode bearer's `(userId, orgId)` — the edge splits `user@org` the same way.
fn identity(token: &str) -> (String, String) {
    match token.split_once('@') {
        Some((user, org)) => (user.to_string(), org.to_string()),
        None => (token.to_string(), String::new()),
    }
}

/// The DeviceRoom's gate: `edge/src/device-room.ts::deviceRoomAccess`.
fn admits(owner: Option<&(String, String)>, caller: &(String, String), is_host: bool) -> bool {
    match owner {
        // Only a device's own backend may claim (and later re-host) its room.
        None => is_host,
        Some(owner) if *owner.0 == caller.0 => true,
        Some(_) if is_host => false,
        // Any other member of the room's org, as a client.
        Some(owner) => !owner.1.is_empty() && owner.1 == caller.1,
    }
}

async fn fake_device_room() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind relay");
    let url = format!(
        "http://127.0.0.1:{}",
        listener.local_addr().expect("addr").port()
    );
    let state = Arc::new(Mutex::new(RelayState::default()));
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let state = state.clone();
            tokio::spawn(async move {
                let mut uri = String::new();
                let gate = state.clone();
                let Ok(ws) = tokio_tungstenite::accept_hdr_async(
                    stream,
                    |req: &WsRequest, res: WsResponse| {
                        uri = req.uri().to_string();
                        let query = uri.split_once('?').map(|(_, q)| q).unwrap_or("");
                        let is_host = query.contains("role=host");
                        let caller = identity(
                            query
                                .split('&')
                                .find_map(|kv| kv.strip_prefix("token="))
                                .unwrap_or(""),
                        );
                        let mut st = gate.lock().expect("lock");
                        if !admits(st.owner.as_ref(), &caller, is_host) {
                            return Err(WsErrorResponse::new(Some("forbidden".to_string())));
                        }
                        if is_host && st.owner.is_none() {
                            st.owner = Some(caller);
                        }
                        Ok(res)
                    },
                )
                .await
                else {
                    return;
                };
                let query = uri.split_once('?').map(|(_, q)| q).unwrap_or("");
                let is_host = query.contains("role=host");
                let conn_id = query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("connId="))
                    .unwrap_or("anon")
                    .to_string();
                let (mut sink, mut ws_stream) = ws.split();
                let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
                {
                    let mut st = state.lock().expect("lock");
                    if is_host {
                        st.host = Some(tx);
                    } else {
                        st.clients.insert(conn_id.clone(), tx);
                    }
                }
                let writer = tokio::spawn(async move {
                    while let Some(bytes) = rx.recv().await {
                        if sink.send(WsMessage::Binary(bytes)).await.is_err() {
                            break;
                        }
                    }
                });
                while let Some(Ok(message)) = ws_stream.next().await {
                    let WsMessage::Binary(bytes) = message else {
                        continue;
                    };
                    let Ok((header, payload)) = decode_device_frame(&bytes) else {
                        break;
                    };
                    let st = state.lock().expect("lock");
                    if is_host {
                        let Some(to) = header.to else { continue };
                        if let Some(client) = st.clients.get(&to) {
                            let stripped = DeviceFrameHeader::new(header.s, header.k);
                            let _ = client
                                .send(encode_device_frame(&stripped, &payload).expect("encode"));
                        }
                    } else if let Some(host) = &st.host {
                        let mut routed = DeviceFrameHeader::new(header.s, header.k);
                        routed.from = Some(conn_id.clone());
                        let _ = host.send(encode_device_frame(&routed, &payload).expect("encode"));
                    }
                }
                writer.abort();
            });
        }
    });
    (url, task)
}

// ---------------------------------------------------------------------------
// Engine fixtures
// ---------------------------------------------------------------------------

/// Instant mock harness so a forwarded QueueCommand fully executes on the target.
struct InstantHarness;

#[async_trait]
impl Harness for InstantHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Instant"
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
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        Ok(futures::stream::iter([
            Ok(AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "instant-1".into(),
                tools: vec![],
                cwd: "/tmp".into(),
                session_id: "hs-1".into(),
                assistant_message_id: "a-1".into(),
            }),
            Ok(AgentEvent::TextDelta {
                text: "remote reply".into(),
            }),
            Ok(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some("hs-1".into()),
            }),
        ])
        .boxed())
    }
}

fn registry() -> Arc<HarnessRegistry> {
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(InstantHarness));
    Arc::new(registry)
}

/// The org every device in these tests belongs to (the box and the laptops
/// driving it), and the two people in it.
const ORG: &str = "org-relay";
const OWNER: &str = "alice";
const TEAMMATE: &str = "bob";

fn bearer(user: &str, org: &str) -> String {
    format!("{user}@{org}")
}

fn assemble(dir: &std::path::Path, device_id: &str) -> EngineCore {
    assemble_as(dir, device_id, OWNER, ORG)
}

/// Assemble an engine signed in as `user` of `org` — the identity its host
/// relay presents to the device room, which is what the room's gate judges.
fn assemble_as(dir: &std::path::Path, device_id: &str, user: &str, org: &str) -> EngineCore {
    std::fs::create_dir_all(dir).expect("create data dir");
    std::fs::write(dir.join("device-id"), device_id).expect("write device id");
    let core =
        EngineCore::assemble(dir, registry(), HarnessId::Mock, None).expect("engine assembles");
    let mut auth = comet_engine::AuthConfig::new("http://localhost:27640", dir.join("auth"));
    auth.dev_user_id = bearer(user, org);
    core.set_auth(comet_engine::Auth::new(auth));
    core
}

/// A link cache dialing the relay as `user` of `org`, at test speed.
fn links(relay_url: &str, user: &str, org: &str) -> Arc<LinkCache> {
    let mut config = LinkCacheConfig::new(
        relay_url.to_string(),
        Arc::new(StaticToken(bearer(user, org))),
    );
    config.probe_timeout = Duration::from_secs(5);
    // The production curve backs a failed dial off 5s→60s, which under a loaded
    // test binary turns "the host relay hasn't finished joining" into minutes of
    // refusals. Retry at test speed instead.
    config.cooldown_base = Duration::from_millis(50);
    config.cooldown_max = Duration::from_millis(200);
    LinkCache::new(config)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn target_device_id_routes_over_the_relay() {
    let (relay_url, _relay) = fake_device_room().await;
    let dirs = tempfile::tempdir().expect("tempdir");

    // Engine B hosts its device room on the fake relay.
    let core_b = assemble(&dirs.path().join("b"), "device-b");
    let _host = core_b.start_host_relay(&relay_url);

    // Engine A dials peers through the same relay.
    let core_a = assemble(&dirs.path().join("a"), "device-a");
    core_a.set_links(links(&relay_url, OWNER, ORG));

    // Seed a transcript on B only — proves reads come from B, not A's (empty) doc.
    let handle_b = core_b.doc_host.open("chat-remote").expect("open chat on B");
    handle_b
        .write_user_message("m-b-1", "hello from B", 1_000)
        .expect("write user message");

    let client = comet_rpc::memory_client(core_a.rpc_service());

    // Our own id in targetDeviceId: handled locally, no forward.
    let local = client
        .call(
            methods::LIST_HARNESSES,
            serde_json::json!({ "targetDeviceId": "device-a" }),
        )
        .await
        .expect("local list");
    assert!(local.is_array());

    // Unary forward: ListHarnesses answered by B through the relay. (The host relay
    // dials with backoff; retry until its session is up.)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let remote = loop {
        match client
            .call(
                methods::LIST_HARNESSES,
                serde_json::json!({ "targetDeviceId": "device-b" }),
            )
            .await
        {
            Ok(value) => break value,
            Err(err) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "relay never came up: {err}"
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    };
    assert!(remote.is_array());

    // The add-space picker's exact call: browse a folder ON B from A's IPC
    // surface (ListFolders + targetDeviceId, relay-forwarded).
    let browse_dir = dirs.path().join("b-folders");
    std::fs::create_dir_all(browse_dir.join("project-x")).expect("browse fixture");
    let listing = client
        .call(
            methods::LIST_FOLDERS,
            serde_json::json!({
                "path": browse_dir.to_string_lossy(),
                "targetDeviceId": "device-b",
            }),
        )
        .await
        .expect("remote ListFolders");
    let names: Vec<&str> = listing["entries"]
        .as_array()
        .expect("entries array")
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    assert!(
        names.contains(&"project-x"),
        "remote folder listing must come from B's filesystem: {names:?}"
    );

    // Streaming proxy: WatchDocMessages against B's doc from A's IPC surface.
    let mut stream = client
        .subscribe(
            methods::WATCH_DOC_MESSAGES,
            serde_json::json!({ "chatId": "chat-remote", "targetDeviceId": "device-b" }),
        )
        .await
        .expect("remote subscribe");
    // The watch emits its current value first ([] if B's publish pass hasn't run yet),
    // then re-emits on every doc change — read until B's entry arrives.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let item = tokio::time::timeout_at(deadline, stream.recv())
            .await
            .expect("remote transcript before timeout")
            .expect("stream alive");
        if item.to_string().contains("hello from B") {
            break;
        }
    }

    // Unary forward with side effects: QueueCommand lands (and executes) on B.
    let command = serde_json::to_value(SessionCommandPayload::Run {
        request: RunRequest {
            prompt: "run remotely".into(),
            model: None,
            reasoning: None,
            model_options: serde_json::Map::new(),
            cwd: "/tmp".into(),
            sandbox: SandboxLevel::WorkspaceWrite,
            auto_approve: true,
            attachments: Vec::new(),
            resume: None,
        },
        message_id: "m-a-1".into(),
    })
    .expect("serialize command");
    let queued = client
        .call(
            methods::QUEUE_COMMAND,
            serde_json::json!({
                "chatId": "chat-remote",
                "targetDeviceId": "device-b",
                "command": command,
            }),
        )
        .await
        .expect("queue on B");
    let command_id = queued["commandId"]
        .as_str()
        .expect("command id")
        .to_string();
    let commands = handle_b.doc().read_commands().expect("read B commands");
    assert!(
        commands.iter().any(|c| c.id == command_id),
        "command must live in B's doc"
    );

    core_a.shutdown().await;
    core_b.shutdown().await;
}

/// M5: terminals are device-addressable — OpenTerminal/WriteTerminal forward as
/// unary calls and SubscribeTerminal proxies its stream through the relay.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_stream_proxies_over_the_relay() {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;

    let (relay_url, _relay) = fake_device_room().await;
    let dirs = tempfile::tempdir().expect("tempdir");
    let cwd = dirs.path().join("work");
    std::fs::create_dir_all(&cwd).expect("cwd");

    // Engine B hosts its device room; its chat row (via its space) pins the
    // terminal cwd.
    let core_b = assemble(&dirs.path().join("b"), "device-b");
    core_b
        .workspace
        .create_space(
            "space-term",
            "device-b",
            &cwd.to_string_lossy(),
            None,
            false,
        )
        .expect("space row on B");
    core_b
        .workspace
        .create_chat("chat-term", "space-term", None, None)
        .expect("chat row on B");
    let _host = core_b.start_host_relay(&relay_url);

    let core_a = assemble(&dirs.path().join("a"), "device-a");
    core_a.set_links(links(&relay_url, OWNER, ORG));
    let client = comet_rpc::memory_client(core_a.rpc_service());

    // OpenTerminal forwards to B once the relay session is up.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let session = loop {
        match client
            .call(
                methods::OPEN_TERMINAL,
                serde_json::json!({
                    "chatId": "chat-term",
                    "cols": 80,
                    "rows": 24,
                    "targetDeviceId": "device-b",
                }),
            )
            .await
        {
            Ok(session) => break session,
            Err(err) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "relay never came up: {err}"
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    };
    let terminal_id = session["id"].as_str().expect("terminal id").to_string();
    assert_eq!(
        session["cwd"].as_str(),
        Some(&*cwd.to_string_lossy()),
        "cwd from B's chat row"
    );

    // SubscribeTerminal: the stream is proxied item-by-item through the relay.
    let mut stream = client
        .subscribe(
            methods::SUBSCRIBE_TERMINAL,
            serde_json::json!({ "terminalId": terminal_id, "targetDeviceId": "device-b" }),
        )
        .await
        .expect("remote subscribe");
    client
        .call(
            methods::WRITE_TERMINAL,
            serde_json::json!({
                "terminalId": terminal_id,
                "data": BASE64.encode("echo r3lay-$((20+2))\n"),
                "targetDeviceId": "device-b",
            }),
        )
        .await
        .expect("remote write");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut transcript = Vec::new();
    loop {
        let item = tokio::time::timeout_at(deadline, stream.recv())
            .await
            .expect("proxied terminal output before timeout")
            .expect("stream alive");
        if item["type"] == "data" {
            let bytes = BASE64
                .decode(item["data"].as_str().expect("data"))
                .expect("valid base64");
            transcript.extend(bytes);
        }
        if String::from_utf8_lossy(&transcript).contains("r3lay-22") {
            break;
        }
    }

    client
        .call(
            methods::CLOSE_TERMINAL,
            serde_json::json!({ "terminalId": terminal_id, "targetDeviceId": "device-b" }),
        )
        .await
        .expect("remote close");

    core_a.shutdown().await;
    core_b.shutdown().await;
}

/// gh#55: the board is device-addressable. One box hosts the board store;
/// every other device drives that board over the relay — `WatchBoard` proxies
/// its stream, and the verbs (`ListBoardRuntimes`, `DispatchTask`,
/// `CancelTask`) forward as unary calls.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_rpcs_forward_to_the_device_hosting_the_board() {
    use comet_board::config::Paths;
    use comet_board::db::{Db, UpsertTask};
    use comet_board::model::{Source, UpstreamState};

    let (relay_url, _relay) = fake_device_room().await;
    let dirs = tempfile::tempdir().expect("tempdir");

    // Engine B is the box: it hosts the board service. Seed a task into its
    // store first, so a forwarded frame is provably B's board and not an empty
    // one A could have produced by itself.
    let board_dir = dirs.path().join("board-b");
    let paths = Paths::under(&board_dir).expect("board paths");
    {
        let db = Db::open(&paths.db()).expect("board store");
        db.upsert_task(&UpsertTask {
            id: "task-on-the-box".into(),
            source: Source::Github,
            source_id: "1".into(),
            identifier: "gh#55".into(),
            title: "relay-forward the board RPCs".into(),
            body: None,
            url: "https://github.com/o/r/issues/55".into(),
            labels: vec![],
            source_state: Some("open".into()),
            linear_team: None,
            linear_project: None,
            upstream: UpstreamState::Unstarted,
            updated_at: "2026-08-04T09:00:00Z".into(),
        })
        .expect("seed task");
    }

    let core_b = assemble(&dirs.path().join("b"), "device-b");
    let runtime_b = Arc::new(comet_engine::CometRuntime::new(
        core_b.repos.clone(),
        core_b.workspace.clone(),
        core_b.doc_host.clone(),
        core_b
            .workspace
            .merged_sessions_watch(core_b.sessions.watch_sessions()),
        core_b.sessions.journal(),
        core_b.agent_accounts.clone(),
        tokio::runtime::Handle::current(),
    ));
    let board_b = comet_engine::BoardService::spawn_at(
        paths,
        core_b
            .workspace
            .merged_sessions_watch(core_b.sessions.watch_sessions()),
        runtime_b,
        core_b.workspace.watch_spaces(),
        tokio::runtime::Handle::current(),
    )
    .expect("board service on B");
    core_b.set_board(Arc::new(board_b));
    let _host = core_b.start_host_relay(&relay_url);

    // Engine A is a teammate's laptop: no board of its own.
    let core_a = assemble(&dirs.path().join("a"), "device-a");
    core_a.set_links(links(&relay_url, OWNER, ORG));
    let client = comet_rpc::memory_client(core_a.rpc_service());

    // Locally, A has nothing to show: the engine refuses the subscription, so
    // the stream closes without ever delivering a frame. That silence is
    // exactly the signal a viewport sweeps on (`view::board::host_candidates`).
    let mut local = client
        .subscribe(methods::WATCH_BOARD, serde_json::json!({}))
        .await
        .expect("subscribe is accepted");
    assert!(
        local.recv().await.is_none(),
        "a device with no board must deliver no rows"
    );

    // Wait for B's host relay to finish joining (it dials with backoff) on a
    // unary call, where a failure is an error rather than a silent stream end.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while let Err(err) = client
        .call(
            methods::LIST_HARNESSES,
            serde_json::json!({ "targetDeviceId": "device-b" }),
        )
        .await
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "relay never came up: {err}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Pointed at the box, the same subscription streams B's rows.
    let mut stream = client
        .subscribe(
            methods::WATCH_BOARD,
            serde_json::json!({ "targetDeviceId": "device-b" }),
        )
        .await
        .expect("remote subscribe");
    let rows = loop {
        let value = tokio::time::timeout_at(deadline, stream.recv())
            .await
            .expect("B's board reaches A before the timeout")
            .expect("the proxied stream stays alive");
        if value.to_string().contains("gh#55") {
            break value;
        }
    };
    assert_eq!(
        rows[0]["id"].as_str(),
        Some("task-on-the-box"),
        "the rows are B's board: {rows}"
    );

    // The verbs forward too. `ListBoardRuntimes` is a static catalog, so its
    // reply proves routing rather than board health.
    let runtimes = client
        .call(
            methods::LIST_BOARD_RUNTIMES,
            serde_json::json!({ "targetDeviceId": "device-b" }),
        )
        .await
        .expect("remote ListBoardRuntimes");
    assert!(
        runtimes.as_array().is_some_and(|r| !r.is_empty()),
        "got: {runtimes}"
    );

    // Dispatch and cancel reach B's board loop: the task has no route, so B
    // refuses it — from B, with B's reason. A alone would have said "board
    // unavailable", which is what makes this an answer and not a local refusal.
    let refused = client
        .call(
            methods::DISPATCH_TASK,
            serde_json::json!({ "taskId": "task-on-the-box", "targetDeviceId": "device-b" }),
        )
        .await
        .expect_err("an unroutable task is refused");
    assert!(
        !refused.to_string().contains("board unavailable"),
        "the refusal must come from B's board, not A's absent one: {refused}"
    );
    let cancelled = client
        .call(
            methods::CANCEL_TASK,
            serde_json::json!({ "taskId": "task-on-the-box", "targetDeviceId": "device-b" }),
        )
        .await;
    if let Err(err) = &cancelled {
        assert!(
            !err.to_string().contains("board unavailable"),
            "cancel must reach B's board: {err}"
        );
    }

    // gh#75: so does the config. `routing.toml` is a file on B's disk, and A
    // is the teammate with no ssh account on B — reading and writing it over
    // the relay is the whole point.
    let routing_path = Paths::under(&board_dir).expect("board paths").routing();
    let read = client
        .call(
            methods::READ_BOARD_CONFIG,
            serde_json::json!({ "targetDeviceId": "device-b" }),
        )
        .await
        .expect("remote ReadBoardConfig");
    assert_eq!(
        read["routing"]["path"].as_str(),
        Some(routing_path.display().to_string().as_str()),
        "the path is B's, not A's: {read}"
    );
    assert_eq!(
        read["routing"]["exists"].as_bool(),
        Some(false),
        "nothing has written B a routing.toml yet: {read}"
    );

    // A writes B's first config, and it lands on B's disk.
    let text = "[[route]]\nmatch = { gh_repo = \"o/r\" }\nworkspace = \"box\"\n\
                repo = \"/tmp\"\nruntime = \"claude-code\"\n";
    let wrote = client
        .call(
            methods::WRITE_BOARD_CONFIG,
            serde_json::json!({
                "op": "text", "text": text, "targetDeviceId": "device-b"
            }),
        )
        .await
        .expect("remote WriteBoardConfig");
    assert_eq!(
        wrote["routing"]["config"]["route"][0]["workspace"].as_str(),
        Some("box"),
        "the reply is a fresh read of what landed: {wrote}"
    );
    assert_eq!(
        std::fs::read_to_string(&routing_path).expect("B's routing.toml"),
        text,
        "the file B's board loop reads is the one A wrote"
    );

    // And a targeted edit that would not validate is refused by B, with B's
    // reason, leaving B's file exactly as it was.
    let refused = client
        .call(
            methods::WRITE_BOARD_CONFIG,
            serde_json::json!({
                "op": "route", "route": 0, "key": "runtime", "value": "nonesuch",
                "targetDeviceId": "device-b"
            }),
        )
        .await
        .expect_err("an unknown runtime is refused");
    assert!(
        refused.to_string().contains("not a comet harness"),
        "the refusal names what it would have broken: {refused}"
    );
    assert_eq!(
        std::fs::read_to_string(&routing_path).expect("B's routing.toml"),
        text,
        "a refused edit leaves the config the board is running on untouched"
    );

    // A valid one lands.
    let set = client
        .call(
            methods::WRITE_BOARD_CONFIG,
            serde_json::json!({
                "op": "route", "route": 0, "key": "max_duration", "value": "6h",
                "targetDeviceId": "device-b"
            }),
        )
        .await
        .expect("remote route edit");
    assert_eq!(
        set["routing"]["config"]["route"][0]["max_duration"].as_str(),
        Some("6h"),
        "got: {set}"
    );

    // gh#104: the pin is a `routing.toml` key like any other, so A can set it
    // on B's board — and B's `WatchBoardOrchestrator` says so at once rather
    // than on its next reread, because pinning is a click and not a poll.
    let mut pinned = client
        .subscribe(
            methods::WATCH_BOARD_ORCHESTRATOR,
            serde_json::json!({ "targetDeviceId": "device-b" }),
        )
        .await
        .expect("remote WatchBoardOrchestrator");
    let first = pinned.recv().await.expect("the current value first");
    assert!(
        first["chatId"].is_null(),
        "nothing is pinned on B yet: {first}"
    );

    client
        .call(
            methods::WRITE_BOARD_CONFIG,
            serde_json::json!({
                "op": "default", "key": "orchestrator_chat", "value": "chat-boss",
                "targetDeviceId": "device-b"
            }),
        )
        .await
        .expect("remote pin");
    let next = tokio::time::timeout(std::time::Duration::from_secs(5), pinned.recv())
        .await
        .expect("the pin is published without waiting for a sync cycle")
        .expect("a frame");
    assert_eq!(next["chatId"].as_str(), Some("chat-boss"), "got: {next}");

    // And the kill switch: unpinning removes the key and the stream says so.
    client
        .call(
            methods::WRITE_BOARD_CONFIG,
            serde_json::json!({
                "op": "default", "key": "orchestrator_chat",
                "targetDeviceId": "device-b"
            }),
        )
        .await
        .expect("remote unpin");
    let cleared = tokio::time::timeout(std::time::Duration::from_secs(5), pinned.recv())
        .await
        .expect("unpinning is published too")
        .expect("a frame");
    assert!(cleared["chatId"].is_null(), "got: {cleared}");

    core_a.shutdown().await;
    core_b.shutdown().await;
}

/// gh#66: the relay carries frames between the devices of ONE ORG — the claim
/// `forwardable()` in `rpc.rs` makes, and until this shipped the room was
/// claim-on-first-join per USER, so a second teammate's laptop was refused
/// before a frame ever reached the box. Everything device-addressed rides this
/// one link, the board included.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_relay_admits_the_org_and_refuses_everyone_else() {
    let (relay_url, _relay) = fake_device_room().await;
    let dirs = tempfile::tempdir().expect("tempdir");

    // The always-on box, signed in as the person who set it up.
    let core_box = assemble_as(&dirs.path().join("box"), "device-box", OWNER, ORG);
    let _host = core_box.start_host_relay(&relay_url);

    // A teammate's laptop: a DIFFERENT WorkOS user in the same org.
    let core_mate = assemble_as(&dirs.path().join("mate"), "device-mate", TEAMMATE, ORG);
    core_mate.set_links(links(&relay_url, TEAMMATE, ORG));
    let mate = comet_rpc::memory_client(core_mate.rpc_service());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let reached = loop {
        match mate
            .call(
                methods::LIST_HARNESSES,
                serde_json::json!({ "targetDeviceId": "device-box" }),
            )
            .await
        {
            Ok(value) => break value,
            Err(err) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "a teammate must reach the box: {err}"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    };
    assert!(reached.is_array(), "answered by the box: {reached}");

    // Somebody from another org, with the room provably live: refused.
    let core_outsider = assemble_as(
        &dirs.path().join("outsider"),
        "device-out",
        "mallory",
        "org-elsewhere",
    );
    core_outsider.set_links(links(&relay_url, "mallory", "org-elsewhere"));
    let outsider = comet_rpc::memory_client(core_outsider.rpc_service());
    outsider
        .call(
            methods::LIST_HARNESSES,
            serde_json::json!({ "targetDeviceId": "device-box" }),
        )
        .await
        .expect_err("another org must not reach the box");

    core_box.shutdown().await;
    core_mate.shutdown().await;
    core_outsider.shutdown().await;
}

#[tokio::test]
async fn remote_target_without_links_fails_clearly() {
    let dirs = tempfile::tempdir().expect("tempdir");
    let core = assemble(&dirs.path().join("solo"), "device-solo");
    let client = comet_rpc::memory_client(core.rpc_service());
    let err = client
        .call(
            methods::LIST_HARNESSES,
            serde_json::json!({ "targetDeviceId": "device-elsewhere" }),
        )
        .await
        .expect_err("offline forward must fail");
    assert!(
        err.to_string().contains("remote routing unavailable"),
        "got: {err}"
    );
    core.shutdown().await;
}
