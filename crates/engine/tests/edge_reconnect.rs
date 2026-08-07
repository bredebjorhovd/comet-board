//! gh#116 — a dropped edge connection comes back on its own.
//!
//! The incident: an edge redeploy at 10:06 cycled the Durable Objects, every
//! one of the box's edge sockets died, and the engine ran on for 25 minutes
//! looking healthy. Locally it was healthy — `comet-board list` answered,
//! dispatches ran. Remotely it did not exist: the iOS host sweep correctly
//! reported that nobody hosts a board. A daemon restart re-joined everything
//! instantly, which is the tell: nothing was broken except that nothing had
//! tried again.
//!
//! Every edge deploy cycles the DOs, so this is a routine event. These tests
//! reproduce it against a fake edge that speaks the real wire protocols —
//! loro-protocol rooms for the workspace/registry/session rooms, the
//! `{s,k,to,from}` device-room codec for the host relay — and can drop every
//! live socket on command, exactly as a redeploy does. What they assert is that
//! the engine comes back WITHOUT a restart, and that
//! [`comet_proto::EdgeHealth`] tells the truth throughout: dark while the
//! sockets are gone, live again after.

// tungstenite's `accept_hdr_async` callback signature fixes the Err type as a
// full `Response` — its size is not ours to shrink.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use loro::{ExportMode, LoroDoc, VersionVector};
use loro_protocol::{
    BatchId, CrdtType, Permission, ProtocolMessage, UpdateStatusCode, decode, encode,
};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::handshake::server::{
    Request as WsRequest, Response as WsResponse,
};

use comet_engine::{EdgeConfig, EngineCore, HarnessRegistry, default_registry};
use comet_proto::HarnessId;

const ORG: &str = "org-edge";
const USER: &str = "alice";
const TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// A fake edge that can be redeployed under a running engine
// ---------------------------------------------------------------------------

/// One accepted socket, and the handle that kills it.
struct LiveSocket {
    /// Dropping this ends the socket's writer; aborting the reader ends the
    /// session. Together they are what a DO handover looks like to a client:
    /// the connection simply goes away.
    writer: tokio::task::JoinHandle<()>,
    reader: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
struct EdgeState {
    sockets: Vec<LiveSocket>,
    /// Per-room doc, so a rejoining client gets real backfill rather than an
    /// empty answer that would hide a broken resync.
    docs: HashMap<String, LoroDoc>,
    /// Is the DeviceRoom's host socket claimed right now? This is the fact the
    /// iOS host sweep reads through `GET /device/{id}/status`, and the one that
    /// was false-and-invisible for 25 minutes.
    host_connected: bool,
}

struct FakeEdge {
    url: String,
    state: Arc<Mutex<EdgeState>>,
    /// Room joins answered, by room id — the observable for "it rejoined".
    joins: Arc<Mutex<HashMap<String, usize>>>,
    /// Device-room host connections accepted.
    host_dials: Arc<AtomicUsize>,
    _accept: tokio::task::JoinHandle<()>,
}

impl FakeEdge {
    fn joins(&self, room_id: &str) -> usize {
        self.joins
            .lock()
            .expect("lock")
            .get(room_id)
            .copied()
            .unwrap_or(0)
    }

    fn host_connected(&self) -> bool {
        self.state.lock().expect("lock").host_connected
    }

    /// Redeploy: drop every live socket, exactly as cycling the Durable Objects
    /// does. The edge keeps listening — this is a handover, not an outage.
    fn redeploy(&self) {
        let mut state = self.state.lock().expect("lock");
        for socket in state.sockets.drain(..) {
            socket.reader.abort();
            socket.writer.abort();
        }
        state.host_connected = false;
    }
}

/// The room id the edge derives from a room path, mirroring the real edge's
/// derivation (`ws3/{org}/{user}`, `orgdev1/{org}`, and the chat id itself).
fn room_id_for(path: &str) -> Option<String> {
    let path = path.split('?').next().unwrap_or(path);
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    match parts.as_slice() {
        ["workspace", org, "ws"] => Some(format!("ws3/{org}/{USER}")),
        ["org", org, "devices", "ws"] => Some(format!("orgdev1/{org}")),
        ["session", chat, "ws"] => Some((*chat).to_string()),
        _ => None,
    }
}

async fn fake_edge() -> FakeEdge {
    fake_edge_on(TcpListener::bind("127.0.0.1:0").await.expect("bind edge"))
}

fn fake_edge_on(listener: TcpListener) -> FakeEdge {
    let url = format!(
        "http://127.0.0.1:{}",
        listener.local_addr().expect("addr").port()
    );
    let state = Arc::new(Mutex::new(EdgeState::default()));
    let joins: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    let host_dials = Arc::new(AtomicUsize::new(0));
    let accept = tokio::spawn(accept_loop(
        listener,
        state.clone(),
        joins.clone(),
        host_dials.clone(),
    ));
    FakeEdge {
        url,
        state,
        joins,
        host_dials,
        _accept: accept,
    }
}

async fn accept_loop(
    listener: TcpListener,
    state: Arc<Mutex<EdgeState>>,
    joins: Arc<Mutex<HashMap<String, usize>>>,
    host_dials: Arc<AtomicUsize>,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            break;
        };
        tokio::spawn(serve_socket(
            stream,
            state.clone(),
            joins.clone(),
            host_dials.clone(),
        ));
    }
}

/// One accepted connection: a room socket (loro-protocol) or the device room's
/// host socket. Device-room frames need no answer for these tests — the host
/// socket's mere existence is what is being asserted, and it is exactly what
/// `GET /device/{id}/status` reports to the iOS host sweep.
async fn serve_socket(
    stream: tokio::net::TcpStream,
    state: Arc<Mutex<EdgeState>>,
    joins: Arc<Mutex<HashMap<String, usize>>>,
    host_dials: Arc<AtomicUsize>,
) {
    let mut uri = String::new();
    let Ok(ws) = tokio_tungstenite::accept_hdr_async(stream, |req: &WsRequest, res: WsResponse| {
        uri = req.uri().to_string();
        Ok(res)
    })
    .await
    else {
        return;
    };
    let (mut sink, mut incoming) = ws.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<WsMessage>();
    let writer = tokio::spawn(async move {
        while let Some(message) = out_rx.recv().await {
            if sink.send(message).await.is_err() {
                break;
            }
        }
    });
    if uri.starts_with("/device/") && uri.contains("role=host") {
        host_dials.fetch_add(1, Ordering::SeqCst);
        state.lock().expect("lock").host_connected = true;
    }
    let room = room_id_for(&uri);
    let reader = tokio::spawn({
        let state = state.clone();
        async move {
            while let Some(Ok(message)) = incoming.next().await {
                match message {
                    // The hibernation-safe keepalive pair.
                    WsMessage::Text(_) => {
                        let _ = out_tx.send(WsMessage::Text("pong".into()));
                    }
                    WsMessage::Binary(bytes) => {
                        if let Some(room) = &room {
                            serve_room_frame(&state, &joins, room, &bytes, &out_tx);
                        }
                    }
                    _ => {}
                }
            }
        }
    });
    state
        .lock()
        .expect("lock")
        .sockets
        .push(LiveSocket { writer, reader });
}

/// The subset of `edge/src/session-room.ts` these tests need: answer joins with
/// the room's version vector plus backfill, import and acknowledge updates.
fn serve_room_frame(
    state: &Arc<Mutex<EdgeState>>,
    joins: &Arc<Mutex<HashMap<String, usize>>>,
    room_id: &str,
    bytes: &[u8],
    out: &mpsc::UnboundedSender<WsMessage>,
) {
    let Ok(message) = decode(bytes) else { return };
    let mut guard = state.lock().expect("lock");
    let doc = guard.docs.entry(room_id.to_string()).or_default().clone();
    drop(guard);
    let reply = |message: &ProtocolMessage| {
        if let Ok(bytes) = encode(message) {
            let _ = out.send(WsMessage::Binary(bytes));
        }
    };
    match message {
        ProtocolMessage::JoinRequest {
            crdt: CrdtType::Loro,
            version,
            ..
        } => {
            *joins
                .lock()
                .expect("lock")
                .entry(room_id.into())
                .or_insert(0) += 1;
            reply(&ProtocolMessage::JoinResponseOk {
                crdt: CrdtType::Loro,
                room_id: room_id.into(),
                permission: Permission::Write,
                version: doc.oplog_vv().encode(),
                extra: None,
            });
            let backfill = if version.is_empty() {
                doc.export(ExportMode::Snapshot)
            } else {
                match VersionVector::decode(&version) {
                    Ok(vv) => doc.export(ExportMode::updates(&vv)),
                    Err(_) => doc.export(ExportMode::Snapshot),
                }
            };
            if let Ok(backfill) = backfill
                && !backfill.is_empty()
            {
                reply(&ProtocolMessage::DocUpdate {
                    crdt: CrdtType::Loro,
                    room_id: room_id.into(),
                    updates: vec![backfill],
                    batch_id: BatchId([0; 8]),
                });
            }
        }
        ProtocolMessage::JoinRequest { crdt, .. } => {
            reply(&ProtocolMessage::JoinResponseOk {
                crdt,
                room_id: room_id.into(),
                permission: Permission::Write,
                version: Vec::new(),
                extra: None,
            });
        }
        ProtocolMessage::DocUpdate {
            crdt,
            updates,
            batch_id,
            ..
        } => {
            if crdt == CrdtType::Loro {
                for update in &updates {
                    let _ = doc.import(update);
                }
            }
            reply(&ProtocolMessage::Ack {
                crdt,
                room_id: room_id.into(),
                ref_id: batch_id,
                status: UpdateStatusCode::Ok,
            });
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn registry() -> Arc<HarnessRegistry> {
    Arc::new(default_registry())
}

fn assemble(dir: &std::path::Path, edge_url: &str) -> EngineCore {
    std::fs::create_dir_all(dir).expect("create data dir");
    std::fs::write(dir.join("device-id"), "device-box").expect("write device id");
    EngineCore::assemble_with_identity(
        dir,
        registry(),
        HarnessId::Mock,
        Some(EdgeConfig::with_static_token(
            edge_url,
            format!("{USER}@{ORG}"),
        )),
        ORG,
        USER,
    )
    .expect("engine assembles")
}

async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The gh#116 exit criterion: an edge redeploy no longer requires anyone to
/// notice and restart boxes.
///
/// Kill every live socket under a running engine — the workspace room, the org
/// registry, the chat's session room and the device-room host socket — and
/// assert all four come back, with no restart and nothing prompting them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_edge_redeploy_is_survived_without_a_restart() {
    let edge = fake_edge().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let core = assemble(dir.path(), &edge.url);
    let _relay = core.start_host_relay(&edge.url);
    core.doc_host.open("chat-live").expect("open chat");

    let workspace_room = format!("ws3/{ORG}/{USER}");
    let registry_room = format!("orgdev1/{ORG}");

    wait_until("the first joins", || {
        edge.joins(&workspace_room) >= 1
            && edge.joins(&registry_room) >= 1
            && edge.joins("chat-live") >= 1
            && edge.host_connected()
    })
    .await;
    wait_until("health to report every connection live", || {
        let health = core.edge_health();
        health.live() == health.expected() && health.expected() == 4
    })
    .await;

    let joins_before = (
        edge.joins(&workspace_room),
        edge.joins(&registry_room),
        edge.joins("chat-live"),
    );
    let host_dials_before = edge.host_dials.load(Ordering::SeqCst);

    // 10:06 — the deploy cycles every Durable Object.
    edge.redeploy();

    // Nobody restarts anything. Every room rejoins on its own.
    wait_until("all four connections to come back", || {
        edge.joins(&workspace_room) > joins_before.0
            && edge.joins(&registry_room) > joins_before.1
            && edge.joins("chat-live") > joins_before.2
            && edge.host_dials.load(Ordering::SeqCst) > host_dials_before
            && edge.host_connected()
    })
    .await;

    wait_until("health to report every connection live again", || {
        let health = core.edge_health();
        health.live() == health.expected() && health.expected() == 4
    })
    .await;
    assert!(!core.edge_health().dark());
}

/// The health line itself (`comet status`, `comet-board doctor`): while the
/// sockets are gone the engine must SAY it holds nothing, rather than reporting
/// the clients it happens to be holding. That gap — up, signed in, holding
/// nothing — was visible only in journald.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn health_reports_the_dark_window_rather_than_hiding_it() {
    let edge = fake_edge().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let core = assemble(dir.path(), &edge.url);
    let _relay = core.start_host_relay(&edge.url);

    wait_until("the engine to come online", || {
        let health = core.edge_health();
        health.expected() == 3 && health.live() == 3
    })
    .await;
    assert!(!core.edge_health().dark());

    edge.redeploy();

    // The observable that did not exist before gh#116: the moment of darkness
    // has a name, and it is not "connected".
    wait_until("health to notice the sockets are gone", || {
        core.edge_health().dark()
    })
    .await;
    let summary = core.edge_health().summary();
    assert!(
        summary.contains("holds NO edge connections"),
        "the dark summary must say so plainly: {summary}"
    );

    wait_until("health to clear once the rooms are back", || {
        !core.edge_health().dark()
    })
    .await;
}

/// A chat opened while the edge is unreachable used to be roomless forever: the
/// one-shot join failed, the handle stayed in the open-chats map, and every
/// later `open()` handed back that same roomless handle. Only a restart fixed
/// it — the nudge path cannot, because the chat is already open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chat_opened_while_the_edge_is_down_still_joins_later() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener); // nothing is listening on this port yet
    let dir = tempfile::tempdir().expect("tempdir");
    let core = assemble(dir.path(), &format!("http://127.0.0.1:{port}"));

    let handle = core.doc_host.open("chat-cold").expect("open chat");
    assert!(!handle.connected(), "there is no edge to be connected to");

    // The edge comes up on the same port a moment later (an engine that booted
    // during a deploy, a box that booted before its network).
    let edge = bind_fake_edge_on(port).await;
    wait_until("the chat room to join once the edge exists", || {
        edge.joins("chat-cold") >= 1
    })
    .await;
    wait_until("the handle to report itself connected", || {
        handle.connected()
    })
    .await;
}

/// [`fake_edge`] on a fixed port, for the test that needs the edge to appear
/// only after the engine has already tried and failed. The rebind retries
/// because the engine's own refused dial can hold the port in TIME_WAIT.
async fn bind_fake_edge_on(port: u16) -> FakeEdge {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => return fake_edge_on(listener),
            Err(err) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "could not rebind the test port: {err}"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}
