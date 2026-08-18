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
//!
//! …and it *stamps* that identity onto every client→host frame (`u`/`o`, gh#161), the
//! way `relayedHeader` does in the DO. The box compares against that stamp rather than
//! against anything the caller wrote, so a relay that did not carry it would make the
//! billing guard untestable here — and a room without it is exactly the state gh#161
//! found the real one in.

// tungstenite's `accept_hdr_async` callback signature fixes the Err type as a full
// `Response` — its size is not ours to shrink.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse as WsErrorResponse, Request as WsRequest, Response as WsResponse,
};

use comet_doc::{QueueOp, SessionCommandPayload};
use comet_engine::{EngineCore, HarnessRegistry};
use comet_harness::{Harness, HarnessError, RunControls};
use comet_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel,
    SteeringMode,
};
use comet_rpc::{
    Caller, DeviceFrameHeader, HostRelay, HostRelayConfig, LinkCache, LinkCacheConfig, RpcError,
    RpcReply, RpcService, StaticToken, decode_device_frame, encode_device_frame, methods,
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
                // The identity the (fake) Worker verified for this socket —
                // the one thing the frames it sends will carry that the sender
                // did not write.
                let (caller_user, caller_org) = identity(
                    query
                        .split('&')
                        .find_map(|kv| kv.strip_prefix("token="))
                        .unwrap_or(""),
                );
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
                        // `relayedHeader`: the client's stream and kind, and
                        // everything else the relay's own. Anything the client
                        // put in `u`/`o` is dropped here, unread.
                        let mut routed = DeviceFrameHeader::new(header.s, header.k);
                        routed.from = Some(conn_id.clone());
                        routed.u = Some(caller_user.clone());
                        routed.o = (!caller_org.is_empty()).then(|| caller_org.clone());
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

/// A board's paths under a scratch dir.
///
/// `Paths::under` used to honour the board's two directory variables, so under
/// an environment that sets them — which is exactly the environment an agent
/// dispatched *by a board* runs in — these tests seeded their fixture tasks into
/// the live board's database and wrote their two-line fixture over its
/// `routing.toml`. It is pure since gh#190: a test's board is a directory it
/// made, never the device's, and that is now the constructor's guarantee rather
/// than each caller's discipline.
fn board_paths(dir: &std::path::Path) -> comet_board::config::Paths {
    comet_board::config::Paths::under(dir).expect("board dirs")
}

/// A board service on `paths`, for an engine that has to *host* one — the box
/// in the forwarding tests, and either side of the two-board sweep (gh#343).
fn board_service(
    core: &EngineCore,
    paths: comet_board::config::Paths,
) -> comet_engine::BoardService {
    let runtime = Arc::new(comet_engine::CometRuntime::new(
        core.repos.clone(),
        core.workspace.clone(),
        core.doc_host.clone(),
        core.workspace
            .merged_sessions_watch(core.sessions.watch_sessions()),
        core.sessions.journal(),
        core.agent_accounts.clone(),
        tokio::runtime::Handle::current(),
    ));
    comet_engine::BoardService::spawn_at(
        paths,
        core.workspace
            .merged_sessions_watch(core.sessions.watch_sessions()),
        runtime,
        core.workspace.watch_spaces(),
        tokio::runtime::Handle::current(),
    )
    .expect("board service")
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

/// A real relay-hosted N-1 protocol seam: no BoardStatsSnapshot until its
/// update mutation tears the relay-shaped answer down and returns upgraded.
struct LegacyStatsPeer {
    phase: Arc<AtomicU8>,
    has_board: Arc<AtomicBool>,
    probe_delay_ms: Arc<AtomicU64>,
    managed_home: std::path::PathBuf,
    restart: Mutex<Option<oneshot::Sender<()>>>,
}

#[async_trait]
impl RpcService for LegacyStatsPeer {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        self.handle_as(method, params, &Caller::LOCAL).await
    }

    async fn handle_as(
        &self,
        method: &str,
        params: serde_json::Value,
        _caller: &Caller,
    ) -> Result<RpcReply, RpcError> {
        if matches!(
            method,
            methods::BOARD_STATS_SNAPSHOT | methods::BOARD_STATS | methods::UPDATE_STATUS
        ) {
            let delay = self.probe_delay_ms.load(Ordering::SeqCst);
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
        }
        match method {
            methods::LIST_HARNESSES => RpcReply::value(&serde_json::json!([])),
            methods::BOARD_STATS_SNAPSHOT if self.phase.load(Ordering::SeqCst) == 0 => {
                Err(RpcError::UnknownMethod(method.into()))
            }
            methods::BOARD_STATS_SNAPSHOT if self.phase.load(Ordering::SeqCst) == 1 => {
                Err(RpcError::Transport("relay restarting".into()))
            }
            methods::BOARD_STATS_SNAPSHOT => {
                let mut stats = comet_proto::view::stats::BoardStats::empty(Some(7));
                stats.attempts = 89;
                RpcReply::value(&comet_proto::view::stats::BoardStatsSnapshot {
                    board_id: "legacy-board".into(),
                    host: comet_proto::view::stats::StatsDevice {
                        device_id: "legacy-peer".into(),
                        label: "Tokenmaxxer9000".into(),
                    },
                    stats,
                    merge_basis: Default::default(),
                })
            }
            methods::BOARD_STATS if !self.has_board.load(Ordering::SeqCst) => {
                Err(RpcError::Refused("board not configured".into()))
            }
            methods::BOARD_STATS => {
                let mut stats = comet_proto::view::stats::BoardStats::empty(Some(7));
                stats.attempts = 89;
                RpcReply::value(&stats)
            }
            methods::LIST_FOLDERS if params.get("path").is_none() => {
                RpcReply::value(&serde_json::json!({
                    "path": self.managed_home,
                    "entries": [],
                    "truncated": false
                }))
            }
            methods::LIST_FOLDERS => Err(RpcError::Failed("could not read that folder".into())),
            methods::UPDATE_STATUS => Ok(RpcReply::Stream(
                futures::stream::once(async {
                    serde_json::to_value(comet_update::UpdateStatus {
                        current_version: "0.7.1".into(),
                        // The incident's six-hour cache predates v0.8. ApplyUpdate
                        // refreshes the manifest, so this is deliberately stale.
                        latest_version: Some("0.7.1".into()),
                        update_available: false,
                        checked_at: Some(1),
                        error: None,
                    })
                    .unwrap()
                })
                .boxed(),
            )),
            methods::APPLY_UPDATE => {
                self.phase.store(1, Ordering::SeqCst);
                if let Some(restart) = self.restart.lock().unwrap().take() {
                    let _ = restart.send(());
                }
                futures::future::pending::<Result<RpcReply, RpcError>>().await
            }
            _ => Err(RpcError::UnknownMethod(method.into())),
        }
    }
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

    // Queue watches are streams too: the relay must proxy the initial frame
    // and a later mutation rather than attempting a unary call.
    let mut queue_stream = client
        .subscribe(
            methods::WATCH_DOC_QUEUE,
            serde_json::json!({ "chatId": "chat-remote", "targetDeviceId": "device-b" }),
        )
        .await
        .expect("remote queue subscribe");
    let initial = tokio::time::timeout(Duration::from_secs(5), queue_stream.recv())
        .await
        .expect("initial remote queue frame")
        .expect("queue stream alive");
    assert!(initial.get("paused").is_none() || initial["paused"].is_null());
    client
        .call(
            methods::QUEUE_COMMAND,
            serde_json::json!({
                "chatId": "chat-remote",
                "targetDeviceId": "device-b",
                "command": SessionCommandPayload::QueueControl { op: QueueOp::Pause {} },
            }),
        )
        .await
        .expect("pause remote queue");
    let changed = tokio::time::timeout(Duration::from_secs(5), queue_stream.recv())
        .await
        .expect("changed remote queue frame")
        .expect("queue stream alive");
    assert_eq!(changed["paused"], "user");

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_board_stats_supports_n_minus_one_and_reconnects_after_update() {
    let (relay_url, _relay) = fake_device_room().await;
    let dirs = tempfile::tempdir().expect("tempdir");
    let phase = Arc::new(AtomicU8::new(0));
    let has_board = Arc::new(AtomicBool::new(false));
    let probe_delay_ms = Arc::new(AtomicU64::new(0));
    // A source-shaped process may have a stale managed install beside it. The
    // N-1 protocol cannot prove which executable is running, so this tree must
    // never be enough to advertise ApplyUpdate.
    let managed_home = dirs.path().join("legacy-home");
    let managed_version = managed_home.join(".comet-native/app/0.7.1");
    std::fs::create_dir_all(&managed_version).expect("managed version directory");
    std::fs::write(managed_version.join("comet"), b"fixture").expect("managed executable");
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        &managed_version,
        managed_home.join(".comet-native/app/current"),
    )
    .expect("managed current symlink");
    let (restart_tx, restart_rx) = oneshot::channel();
    let legacy_peer = Arc::new(LegacyStatsPeer {
        phase: phase.clone(),
        has_board: has_board.clone(),
        probe_delay_ms: probe_delay_ms.clone(),
        managed_home,
        restart: Mutex::new(Some(restart_tx)),
    });
    let host_config = || {
        let mut config = HostRelayConfig::new(
            relay_url.clone(),
            "legacy-peer",
            Arc::new(StaticToken(bearer(OWNER, ORG))),
        );
        config.retry = Duration::from_millis(20);
        config
    };
    let legacy_host = HostRelay::spawn(host_config(), legacy_peer.clone(), Arc::new(|_| {}));

    let core = assemble(&dirs.path().join("collector"), "collector");
    core.set_board(Arc::new(board_service(
        &core,
        board_paths(&dirs.path().join("board")),
    )));
    core.set_links(links(&relay_url, OWNER, ORG));
    core.workspace
        .org_devices()
        .upsert_device(&comet_proto::Device {
            id: "legacy-peer".into(),
            name: "Tokenmaxxer9000".into(),
            platform: "linux".into(),
            last_seen_at: None,
            created_at: None,
            version: Some("0.7.1".into()),
        });
    let local = comet_rpc::memory_client(core.rpc_service());
    let refusal = local
        .call(
            methods::APPLY_UPDATE,
            serde_json::json!({ "targetDeviceId": "legacy-peer" }),
        )
        .await
        .expect_err("localhost cannot mint an operator update");
    assert!(refusal.to_string().contains("operator surface"));
    let teammate_refusal = match core
        .rpc_service()
        .handle_as(
            methods::APPLY_UPDATE,
            serde_json::json!({}),
            &Caller::relayed(Some(TEAMMATE.into()), Some(ORG.into())),
        )
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("an org member is not the device owner"),
    };
    assert!(
        teammate_refusal
            .to_string()
            .contains("verified device owner")
    );
    let operator = comet_rpc::operator_memory_client(core.rpc_service());

    // The compatible legacy method distinguishes a reachable engine whose
    // board is disabled from a board that merely needs the new snapshot RPC.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let aggregate = operator
            .call(
                methods::AGGREGATE_BOARD_STATS,
                serde_json::json!({ "sinceDays": 7 }),
            )
            .await;
        let no_board = aggregate
            .ok()
            .and_then(|value| {
                serde_json::from_value::<comet_proto::view::stats::AggregateBoardStats>(value).ok()
            })
            .is_some_and(|aggregate| {
                aggregate.hosts.iter().any(|host| {
                    host.device.device_id == "legacy-peer"
                        && host.status == comet_proto::view::stats::StatsHostStatus::NoBoard
                        && host.upgrade.is_none()
                })
            });
        if no_board {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "legacy disabled board was not classified as noBoard"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    has_board.store(true, Ordering::SeqCst);

    // Snapshot, legacy BoardStats, and UpdateStatus share one five-second
    // deadline. Three delayed steps must not each renew that budget.
    probe_delay_ms.store(2_100, Ordering::SeqCst);
    let started = tokio::time::Instant::now();
    let delayed: comet_proto::view::stats::AggregateBoardStats = serde_json::from_value(
        operator
            .call(
                methods::AGGREGATE_BOARD_STATS,
                serde_json::json!({ "sinceDays": 7 }),
            )
            .await
            .expect("aggregate remains readable after a peer timeout"),
    )
    .expect("aggregate shape");
    assert!(
        started.elapsed() < Duration::from_millis(5_800),
        "compatibility fallbacks renewed the per-device deadline"
    );
    assert!(delayed.hosts.iter().any(|host| {
        host.device.device_id == "legacy-peer"
            && host.status == comet_proto::view::stats::StatsHostStatus::Unreachable
            && host
                .error
                .as_deref()
                .is_some_and(|error| error.contains("timed out"))
    }));
    probe_delay_ms.store(0, Ordering::SeqCst);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let aggregate: Option<comet_proto::view::stats::AggregateBoardStats> = operator
            .call(
                methods::AGGREGATE_BOARD_STATS,
                serde_json::json!({ "sinceDays": 7 }),
            )
            .await
            .ok()
            .and_then(|value| serde_json::from_value(value).ok());
        let unmanaged = aggregate.is_some_and(|aggregate| {
            aggregate.hosts.iter().any(|host| {
                host.device.device_id == "legacy-peer"
                    && host
                        .upgrade
                        .as_ref()
                        .is_some_and(|upgrade| !upgrade.can_apply)
            })
        });
        if unmanaged {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "source-shaped Linux peer incorrectly offered an update"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let before: comet_proto::view::stats::AggregateBoardStats = loop {
        match operator
            .call(
                methods::AGGREGATE_BOARD_STATS,
                serde_json::json!({ "sinceDays": 7 }),
            )
            .await
            .and_then(|value| {
                serde_json::from_value::<comet_proto::view::stats::AggregateBoardStats>(value)
                    .map_err(|error| RpcError::Failed(error.to_string()))
            }) {
            Ok(aggregate)
                if aggregate.hosts.iter().any(|host| {
                    host.device.device_id == "legacy-peer"
                        && host.status == comet_proto::view::stats::StatsHostStatus::UpgradeRequired
                }) =>
            {
                break aggregate;
            }
            result => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "no N-1 answer: {result:?}"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    };
    let upgrade = before
        .hosts
        .iter()
        .find(|host| host.device.device_id == "legacy-peer")
        .and_then(|host| host.upgrade.as_ref())
        .expect("upgrade details");
    assert_eq!(upgrade.current_version, "0.7.1");
    assert!(
        !upgrade.can_apply,
        "a stale managed tree cannot prove the running process is managed"
    );
    assert!(upgrade.error.contains("BoardStatsSnapshot"));

    let update_service = core.rpc_service();
    let mut apply = tokio::spawn(async move {
        update_service
            .handle_as(
                methods::APPLY_UPDATE,
                serde_json::json!({ "targetDeviceId": "legacy-peer" }),
                &Caller::OPERATOR,
            )
            .await
    });
    restart_rx.await.expect("legacy peer began its restart");
    drop(legacy_host);
    phase.store(2, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _restarted_host = HostRelay::spawn(host_config(), legacy_peer, Arc::new(|_| {}));
    match tokio::time::timeout(Duration::from_secs(1), &mut apply).await {
        Ok(Ok(Err(error))) => assert!(
            error.to_string().contains("connection closed"),
            "unexpected lost-reply error: {error}"
        ),
        Err(_) => apply.abort(),
        Ok(Ok(Ok(_))) => panic!("restart must lose the mutation reply"),
        Ok(Err(error)) => panic!("apply task failed: {error}"),
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let aggregate: comet_proto::view::stats::AggregateBoardStats = serde_json::from_value(
            operator
                .call(
                    methods::AGGREGATE_BOARD_STATS,
                    serde_json::json!({ "sinceDays": 7 }),
                )
                .await
                .expect("collector remains available"),
        )
        .expect("aggregate shape");
        let returned = aggregate.hosts.iter().any(|host| {
            host.device.device_id == "legacy-peer"
                && host.status == comet_proto::view::stats::StatsHostStatus::Answered
        });
        if returned {
            assert!(aggregate.stats.attempts >= 89);
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "peer never returned"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
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
    use comet_board::db::{Db, UpsertTask};
    use comet_board::model::{Source, UpstreamState};

    let (relay_url, _relay) = fake_device_room().await;
    let dirs = tempfile::tempdir().expect("tempdir");

    // Engine B is the box: it hosts the board service. Seed a task into its
    // store first, so a forwarded frame is provably B's board and not an empty
    // one A could have produced by itself.
    // Through `board_paths`, whose comment says why the derivation had to stop
    // reading the environment (gh#162, gh#190).
    let board_dir = dirs.path().join("board-b");
    let paths = board_paths(&board_dir);
    {
        let db = Db::open(&paths.db()).expect("board store");
        db.upsert_task(&UpsertTask {
            id: "task-on-the-box".into(),
            source: Source::Github,
            source_id: "1".into(),
            identifier: "gh#55".into(),
            title: "relay-forward the board RPCs".into(),
            // gh#132: the detail read has to bring this back across the relay.
            body: Some("The store is on the box; the panel is on the laptop.".into()),
            url: "https://github.com/o/r/issues/55".into(),
            labels: vec![],
            source_state: Some("open".into()),
            linear_team: None,
            linear_project: None,
            upstream: UpstreamState::Unstarted,
            updated_at: "2026-08-04T09:00:00Z".into(),
        })
        .expect("seed task");
        // …and an attempt to hang a review on (§gh#183): claims belong to a
        // run, and the review verbs forward for the reason every other board
        // verb does — the attempt row is on the box.
        db.insert_attempt(&comet_board::db::NewAttempt {
            stacked_on: None,
            task_id: "task-on-the-box".into(),
            pane_id: Some("chat-55".into()),
            workspace: "offhand".into(),
            runtime: "claude-code".into(),
            worktree: None,
            branch: Some("board/gh-55".into()),
            dispatched_by: None,
            dispatched_by_pane: None,
            base_sha: None,
            account: None,
            repo_path: None,
            dispatched_by_device: None,
            dispatched_by_user: None,
            dispatched_by_verified: false,
            billed_to: None,
        })
        .expect("seed attempt");
    }

    let core_b = assemble(&dirs.path().join("b"), "device-b");
    core_b.set_board(Arc::new(board_service(&core_b, paths)));
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

    // The verbs forward too. `ListBoardRuntimes` is served off the harness
    // catalog rather than the board loop, so its reply proves routing rather
    // than board health — and since gh#187 it answers for the *target* device,
    // which is what makes A's picker honest about what B can start.
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

    // gh#132: and so does the detail read. The issue text is in B's store, and
    // the panel asking for it is on A — the row it draws came over this same
    // relay, so the body has to be able to follow it.
    let detail = client
        .call(
            methods::READ_BOARD_TASK,
            serde_json::json!({ "taskId": "task-on-the-box", "targetDeviceId": "device-b" }),
        )
        .await
        .expect("remote ReadBoardTask");
    assert_eq!(
        detail["id"].as_str(),
        Some("task-on-the-box"),
        "the detail is B's row's: {detail}"
    );
    assert_eq!(
        detail["body"].as_str(),
        Some("The store is on the box; the panel is on the laptop."),
        "the issue text has to cross the relay too, or the panel that drew the \
         row over it cannot read the row: {detail}"
    );
    // And it stays OFF the streamed rows: a hundred bodies per sync cycle is
    // exactly the frame this call exists to avoid.
    assert!(
        rows[0].get("body").is_none(),
        "the body must not ride WatchBoard: {rows}"
    );
    // An id B has never seen is refused by B, naming the row — not by A saying
    // it has no board.
    let missing = client
        .call(
            methods::READ_BOARD_TASK,
            serde_json::json!({ "taskId": "nothing-here", "targetDeviceId": "device-b" }),
        )
        .await
        .expect_err("an unknown row is refused");
    assert!(
        missing.to_string().contains("nothing-here"),
        "the refusal must name the row B looked for: {missing}"
    );

    // §gh#183: and so do the review verbs. The claim contract is enforced on
    // the board's host — the parse, the refusal and the diff all happen where
    // the attempt and its checkout are — so an agent submitting from anywhere
    // else gets the same answer it would get on the box.
    let refused = client
        .call(
            methods::SUBMIT_CLAIMS,
            serde_json::json!({
                "taskId": "task-on-the-box",
                "text": "I improved the thing.",
                "targetDeviceId": "device-b",
            }),
        )
        .await
        .expect_err("prose has no file anchor and is refused");
    assert!(
        refused.to_string().contains("::"),
        "the refusal has to name the format, from B: {refused}"
    );
    let review = client
        .call(
            methods::SUBMIT_CLAIMS,
            serde_json::json!({
                "taskId": "task-on-the-box",
                "text": "Forwarded the board RPCs :: crates/engine/src/rpc.rs",
                "targetDeviceId": "device-b",
            }),
        )
        .await
        .expect("remote SubmitClaims");
    assert_eq!(
        review["claims"][0]["text"].as_str(),
        Some("Forwarded the board RPCs")
    );
    // No checkout on this attempt, so the diff cannot be read — and the review
    // says so rather than answering with an empty one.
    assert_eq!(review["diff"]["source"].as_str(), Some("unavailable"));

    let read = client
        .call(
            methods::READ_ATTEMPT_REVIEW,
            serde_json::json!({ "taskId": "task-on-the-box", "targetDeviceId": "device-b" }),
        )
        .await
        .expect("remote ReadAttemptReview");
    assert_eq!(read["brief"]["identifier"].as_str(), Some("gh#55"));
    assert!(
        read["claimed_at"].is_string(),
        "the claims are stored on B's attempt row: {read}"
    );

    // gh#75: so does the config. `routing.toml` is a file on B's disk, and A
    // is the teammate with no ssh account on B — reading and writing it over
    // the relay is the whole point.
    let routing_path = board_paths(&board_dir).routing();
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

    // gh#162: and so is the `[users]` map, which is the one config edit whose
    // *reason* is that A is a different person from whoever set B up. Until
    // this op existed, mapping a teammate meant hand-writing TOML on the box —
    // and until somebody did, every task that teammate released committed
    // under B's own git identity.
    //
    // An address needs no round trip to GitHub, which is what makes this
    // testable without a credential; a bare login takes B's App, deliberately,
    // because A's laptop has none.
    let mapped = client
        .call(
            methods::WRITE_BOARD_CONFIG,
            serde_json::json!({
                "op": "member", "user": "ana@example.com",
                "github": "22494697+ana@users.noreply.github.com",
                "targetDeviceId": "device-b"
            }),
        )
        .await
        .expect("remote member add");
    assert_eq!(
        mapped["routing"]["config"]["users"]["ana@example.com"].as_str(),
        Some("22494697+ana@users.noreply.github.com"),
        "got: {mapped}"
    );
    assert!(
        std::fs::read_to_string(&routing_path)
            .expect("B's routing.toml")
            .contains("[users]"),
        "the map is on B's disk, where B's dispatch reads it"
    );

    // A value that is not an address is refused by B rather than stamped onto
    // GIT_AUTHOR_EMAIL, where it would produce exactly the unattributable
    // commits the map exists to prevent.
    let refused = client
        .call(
            methods::WRITE_BOARD_CONFIG,
            serde_json::json!({
                "op": "member", "user": "sam@example.com", "github": "Sam Ito",
                "targetDeviceId": "device-b"
            }),
        )
        .await
        .expect_err("a name is not a GitHub identity");
    assert!(
        refused.to_string().contains("GitHub login"),
        "the refusal says what to type instead: {refused}"
    );

    // Offboarding is the same surface from the other side.
    let removed = client
        .call(
            methods::WRITE_BOARD_CONFIG,
            serde_json::json!({
                "op": "user", "user": "ana@example.com", "value": null,
                "targetDeviceId": "device-b"
            }),
        )
        .await
        .expect("remote member remove");
    assert!(
        removed["routing"]["config"]["users"]
            .as_object()
            .is_some_and(|m| m.is_empty()),
        "got: {removed}"
    );

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

/// gh#161: the box compares against an identity the edge verified, not one the
/// frontend typed.
///
/// Two laptops dispatch the same task into the same box under `billing_guard =
/// "require-own"`, and each sends a `viaUser` naming the box's owner — the
/// exact lie §gh#74 said walked straight through. The teammate is refused, and
/// refused *naming them*; the owner's own laptop gets past the guard and fails
/// on the next thing, which is what "past the guard" looks like from outside.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn require_own_refuses_the_teammate_the_relay_names_not_the_user_they_claim() {
    use comet_board::db::{Db, UpsertTask};
    use comet_board::model::{Source, UpstreamState};

    let (relay_url, _relay) = fake_device_room().await;
    let dirs = tempfile::tempdir().expect("tempdir");

    // The box: one task, one route it matches, and a guard set to refuse.
    let board_dir = dirs.path().join("board-box");
    let paths = board_paths(&board_dir);
    {
        let db = Db::open(&paths.db()).expect("board store");
        db.upsert_task(&UpsertTask {
            id: "gh:o/r#161".into(),
            source: Source::Github,
            source_id: "161".into(),
            identifier: "gh#161".into(),
            title: "the box must verify who dispatched".into(),
            body: None,
            url: "https://github.com/o/r/issues/161".into(),
            labels: vec![],
            source_state: Some("open".into()),
            linear_team: None,
            linear_project: None,
            upstream: UpstreamState::Unstarted,
            updated_at: "2026-08-09T09:00:00Z".into(),
        })
        .expect("seed task");
    }
    std::fs::write(
        paths.routing(),
        "[defaults]\nbilling_guard = \"require-own\"\n\n\
         [[route]]\nmatch = { gh_repo = \"o/r\" }\nworkspace = \"box\"\n\
         repo = \"/tmp\"\nruntime = \"mock\"\n",
    )
    .expect("write routing.toml");

    let core_box = assemble_as(&dirs.path().join("box"), "device-box", OWNER, ORG);
    core_box.set_board(Arc::new(board_service(&core_box, paths)));
    let _host = core_box.start_host_relay(&relay_url);

    // The dispatch every laptop below sends: no `account` (the silent default),
    // and a `viaUser` claiming to be the box's owner.
    let dispatch = || {
        serde_json::json!({
            "taskId": "gh:o/r#161",
            "viaUser": bearer(OWNER, ORG),
            "viaDevice": "some-laptop",
            "targetDeviceId": "device-box",
        })
    };

    // A teammate's laptop, in the org — admitted by the relay (gh#66), and
    // that is the point: reaching the box is not the same as spending it.
    let core_mate = assemble_as(&dirs.path().join("mate"), "device-mate", TEAMMATE, ORG);
    core_mate.set_links(links(&relay_url, TEAMMATE, ORG));
    let mate = comet_rpc::memory_client(core_mate.rpc_service());

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let refused = loop {
        let err = mate
            .call(methods::DISPATCH_TASK, dispatch())
            .await
            .expect_err("a teammate's dispatch of somebody else's plan is refused");
        let text = err.to_string();
        // Ride over the host relay still joining, the way the other tests do.
        if !text.contains("unreachable") && !text.contains("readiness check") {
            break text;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "relay never came up: {text}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert!(
        refused.contains(comet_proto::view::board::REQUIRE_OWN_REFUSAL),
        "the refusal has to be the billing guard's, so a frontend can offer the \
         confirm instead of a dead end: {refused}"
    );
    assert!(
        refused.contains(TEAMMATE),
        "the box refuses naming who the edge said this was — not who the frame \
         claimed to be: {refused}"
    );
    assert!(
        !refused.contains(&format!("dispatch came from {OWNER}")),
        "the claimed user must not be what the refusal is about: {refused}"
    );

    // The owner's own laptop, sending the identical params over the identical
    // relay. The stamp differs, so the verdict does: it clears the guard and
    // dies on the next refusal in `handle_dispatch` — the space `box` that this
    // test never created.
    let core_own = assemble_as(&dirs.path().join("own"), "device-own", OWNER, ORG);
    core_own.set_links(links(&relay_url, OWNER, ORG));
    let own = comet_rpc::memory_client(core_own.rpc_service());
    let past_the_guard = own
        .call(methods::DISPATCH_TASK, dispatch())
        .await
        .expect_err("no space named `box` exists in this test");
    assert!(
        past_the_guard.to_string().contains("no comet space named"),
        "the owner's own dispatch must reach the refusal *after* billing: {past_the_guard}"
    );

    core_box.shutdown().await;
    core_mate.shutdown().await;
    core_own.shutdown().await;
}

/// gh#343: the second board on one repo is refused when it is *added*, not
/// discovered by `doctor` afterwards.
///
/// The failure this prevents has no symptom until it has an expensive one:
/// both boards derive the same issue as ready, either dispatches it, and
/// neither records the other's attempt — two agents, two worktrees, two
/// branches on one ticket, each board's row looking perfectly normal until two
/// pull requests appear. This test is the wiring that makes the refusal
/// possible at all: the box asks the other devices what they poll, over the
/// same relay every other board verb rides, before it writes.
///
/// The org registry is written by hand here because nothing in this harness
/// syncs it — in production it is the room that carries the fleet (gh#66), and
/// it is what the sweep enumerates.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repo_another_board_already_polls_is_refused_before_it_is_written() {
    const SHARED: &str = "bredebjorhovd/itsm-agent";

    let (relay_url, _relay) = fake_device_room().await;
    let dirs = tempfile::tempdir().expect("tempdir");

    // The Mac: a board of its own, polling the repo, reachable over the relay.
    let paths_a = board_paths(&dirs.path().join("board-a"));
    std::fs::create_dir_all(paths_a.routing().parent().expect("config dir")).expect("config dir");
    std::fs::write(
        paths_a.routing(),
        format!("[github]\nrepos = [\"{SHARED}\"]\n"),
    )
    .expect("A's routing.toml");
    let core_a = assemble(&dirs.path().join("a"), "device-a");
    core_a.set_board(Arc::new(board_service(&core_a, paths_a)));
    let _host_a = core_a.start_host_relay(&relay_url);

    // The box: its own board, and the links that let it ask.
    let paths_b = board_paths(&dirs.path().join("board-b"));
    let core_b = assemble(&dirs.path().join("b"), "device-b");
    core_b.set_board(Arc::new(board_service(&core_b, paths_b.clone())));
    core_b.set_links(links(&relay_url, OWNER, ORG));
    core_b
        .workspace
        .org_devices()
        .upsert_device(&comet_proto::Device {
            id: "device-a".into(),
            name: "Tokenmaxxer9000".into(),
            platform: "macos".into(),
            last_seen_at: None,
            created_at: None,
            version: None,
        });

    let client = comet_rpc::memory_client(core_b.rpc_service());
    // B starts from a config that parses, so what refuses the adopt below is
    // the other board rather than an unreadable file.
    client
        .call(
            methods::WRITE_BOARD_CONFIG,
            serde_json::json!({
                "op": "text",
                "text": "[[route]]\nmatch = { gh_repo = \"o/r\" }\nworkspace = \"box\"\n\
                         repo = \"/tmp\"\nruntime = \"claude-code\"\n",
            }),
        )
        .await
        .expect("B's first config");

    // A's host relay dials with backoff; wait for it on the call the sweep
    // itself makes, so the assertion below cannot pass by nobody answering.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while let Err(err) = client
        .call(
            methods::READ_BOARD_CONFIG,
            serde_json::json!({ "targetDeviceId": "device-a" }),
        )
        .await
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "A's relay never came up: {err}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let refused = client
        .call(
            methods::WRITE_BOARD_CONFIG,
            serde_json::json!({ "op": "adopt", "slug": SHARED }),
        )
        .await
        .expect_err("the Mac already polls it");
    assert!(
        refused.to_string().contains("Tokenmaxxer9000"),
        "the refusal names the board it is already on, because the fix is \
         deleting a line from one of two files: {refused}"
    );
    assert!(
        refused.to_string().contains("--force"),
        "…and the way to say the sharing is intended: {refused}"
    );
    assert!(
        !std::fs::read_to_string(paths_b.routing())
            .expect("B's routing.toml")
            .contains(SHARED),
        "a refused add leaves the config the board polls on untouched"
    );

    // With `--force`, the sweep is no longer what stands in the way: the adopt
    // proceeds to the ordinary check it would always have hit, since nothing on
    // this device has a checkout of that repo.
    let forced = client
        .call(
            methods::WRITE_BOARD_CONFIG,
            serde_json::json!({ "op": "adopt", "slug": SHARED, "force": true }),
        )
        .await
        .expect_err("no space on B holds that repo");
    assert!(
        forced.to_string().contains("unadopted list"),
        "--force must get past the second board and no further: {forced}"
    );

    core_a.shutdown().await;
    core_b.shutdown().await;
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
