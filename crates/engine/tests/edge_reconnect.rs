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
use loro::awareness::EphemeralStore;
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

/// One socket on a room, as the real `SessionRoom` sees it: who is on the other
/// end (`?deviceId=`), and whether they have joined the `%EPH` sub-room.
struct Member {
    tx: mpsc::UnboundedSender<WsMessage>,
    /// The device this socket belongs to — what presence is DERIVED from
    /// (gh#145). `None` for a socket that named no device, which contributes no
    /// presence, exactly as on the real edge.
    device_id: Option<String>,
    /// Joined `%EPH`, i.e. wants presence broadcasts.
    eph: bool,
}

#[derive(Default)]
struct EdgeState {
    sockets: Vec<LiveSocket>,
    /// Per-room doc, so a rejoining client gets real backfill rather than an
    /// empty answer that would hide a broken resync.
    docs: HashMap<String, LoroDoc>,
    /// Live member sockets per room — what lets one engine's doc updates reach
    /// another engine on the same room, exactly as the real edge broadcasts
    /// them, and what presence is now derived from (gh#145). Dead senders are
    /// tolerated (their sends fail silently) and replaced by the rejoin's
    /// registration.
    members: HashMap<String, Vec<Member>>,
    /// Is the DeviceRoom's host socket claimed right now? This is the fact the
    /// iOS host sweep reads through `GET /device/{id}/status`, and the one that
    /// was false-and-invisible for 25 minutes.
    host_connected: bool,
    /// `%EPH` writes received FROM clients — the frames that used to wake this
    /// object every 15s per device, per room, forever (gh#145). The bill was
    /// this number.
    client_eph_writes: usize,
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

    fn client_eph_writes(&self) -> usize {
        self.state.lock().expect("lock").client_eph_writes
    }

    /// Redeploy: drop every live socket, exactly as cycling the Durable Objects
    /// does. The edge keeps listening — this is a handover, not an outage.
    fn redeploy(&self) {
        let mut state = self.state.lock().expect("lock");
        for socket in state.sockets.drain(..) {
            socket.reader.abort();
            socket.writer.abort();
        }
        // Cycling the Durable Objects takes their socket sets with them, which
        // since gh#145 is where presence lives — a redeployed room knows nobody
        // until they dial back in.
        state.members.clear();
        state.host_connected = false;
    }

    /// A room-generation break (gh#148): the Worker starts naming a room
    /// `ws4/…` where it said `ws3/…`, so `idFromName` resolves to a DIFFERENT
    /// Durable Object — one whose storage has never been written. Modelled
    /// here as what it actually is from the client's side: the room this
    /// device syncs into answers with an empty doc, and every socket cycles.
    ///
    /// Deliberately NOT the same call as [`Self::redeploy`]. A redeploy keeps
    /// the storage; this abandons it. The whole question of the ticket is
    /// whether the second one is survivable, so it has to be expressible.
    fn abandon_room_storage(&self, room_id: &str) {
        self.state.lock().expect("lock").docs.remove(room_id);
        self.redeploy();
    }

    /// Read a room's SERVER-side doc as a workspace doc — what a device that
    /// has never seen this workspace before would be backfilled with.
    fn room_chat_ids(&self, room_id: &str) -> Vec<String> {
        let Some(doc) = self.state.lock().expect("lock").docs.get(room_id).cloned() else {
            return Vec::new();
        };
        let Ok(snapshot) = doc.export(ExportMode::Snapshot) else {
            return Vec::new();
        };
        let mirror = LoroDoc::new();
        if mirror.import(&snapshot).is_err() {
            return Vec::new();
        }
        comet_doc::WorkspaceDoc::from_doc(mirror)
            .read_chats()
            .map(|chats| chats.into_iter().map(|chat| chat.id).collect())
            .unwrap_or_default()
    }
}

/// The room id the edge derives from a room path, mirroring the real edge's
/// derivation (`ws4/{org}/{user}`, `orgdev1/{org}`, and the chat id itself).
fn room_id_for(path: &str) -> Option<String> {
    let path = path.split('?').next().unwrap_or(path);
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    match parts.as_slice() {
        ["workspace", org, "ws"] => Some(format!("ws4/{org}/{USER}")),
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
    if let Some(room) = &room {
        state
            .lock()
            .expect("lock")
            .members
            .entry(room.clone())
            .or_default()
            .push(Member {
                tx: out_tx.clone(),
                device_id: device_id_of(&uri),
                eph: false,
            });
    }
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
            // The socket left: drop it from the room's set and re-answer, which
            // is the whole offline path now that nothing beats (gh#145).
            if let Some(room) = &room {
                state
                    .lock()
                    .expect("lock")
                    .members
                    .entry(room.clone())
                    .or_default()
                    .retain(|member| !member.tx.same_channel(&out_tx));
                publish_presence(&state, room);
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
            if crdt == CrdtType::LoroEphemeralStore {
                // A presence joiner: remember it wants broadcasts, then answer
                // with who this room can see — derived from the socket set, the
                // way `SessionRoom::publishPresence` does since gh#145.
                if let Some(members) = state.lock().expect("lock").members.get_mut(room_id) {
                    for member in members.iter_mut() {
                        if member.tx.same_channel(out) {
                            member.eph = true;
                        }
                    }
                }
                publish_presence(state, room_id);
            }
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
            if crdt == CrdtType::LoroEphemeralStore {
                state.lock().expect("lock").client_eph_writes += 1;
            }
            reply(&ProtocolMessage::Ack {
                crdt,
                room_id: room_id.into(),
                ref_id: batch_id,
                status: UpdateStatusCode::Ok,
            });
            // Broadcast to every other member of the room — the path a box's
            // presence heartbeat (and its device-row write) takes to reach the
            // engine that renders its online dot (gh#126).
            let peers: Vec<mpsc::UnboundedSender<WsMessage>> = state
                .lock()
                .expect("lock")
                .members
                .get(room_id)
                .map(|members| {
                    members
                        .iter()
                        .filter(|member| !member.tx.same_channel(out))
                        .map(|member| member.tx.clone())
                        .collect()
                })
                .unwrap_or_default();
            for peer in peers {
                let _ = peer.send(WsMessage::Binary(bytes.to_vec()));
            }
        }
        _ => {}
    }
}

/// The `?deviceId=` a room socket named itself with, if any — what the real
/// edge stamps onto the socket attachment so presence can be derived from it.
fn device_id_of(uri: &str) -> Option<String> {
    uri.split('?')
        .nth(1)?
        .split('&')
        .find_map(|pair| pair.strip_prefix("deviceId="))
        .map(str::to_string)
}

/// Answer the room's presence question the way `SessionRoom` does since gh#145:
/// look at who is actually attached, write one `presence/{deviceId}` entry per
/// device, and push the whole thing to every `%EPH` member.
///
/// Nothing here is on a timer, which is the point — this fires on a join and on
/// a socket going away, both of which are events the real Durable Object was
/// already awake for.
fn publish_presence(state: &Arc<Mutex<EdgeState>>, room_id: &str) {
    let (present, targets) = {
        let guard = state.lock().expect("lock");
        let Some(members) = guard.members.get(room_id) else {
            return;
        };
        let present: Vec<String> = members
            .iter()
            .filter_map(|member| member.device_id.clone())
            .collect();
        let targets: Vec<mpsc::UnboundedSender<WsMessage>> = members
            .iter()
            .filter(|member| member.eph)
            .map(|member| member.tx.clone())
            .collect();
        (present, targets)
    };
    if targets.is_empty() {
        return;
    }
    let store = EphemeralStore::new(30_000);
    let now = chrono::Utc::now().timestamp_millis();
    for device_id in &present {
        store.set(&format!("presence/{device_id}"), now);
    }
    let encoded = store.encode_all();
    if encoded.is_empty() {
        return;
    }
    let Ok(bytes) = encode(&ProtocolMessage::DocUpdate {
        crdt: CrdtType::LoroEphemeralStore,
        room_id: room_id.into(),
        updates: vec![encoded],
        batch_id: BatchId([0; 8]),
    }) else {
        return;
    };
    for target in targets {
        let _ = target.send(WsMessage::Binary(bytes.clone()));
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn registry() -> Arc<HarnessRegistry> {
    Arc::new(default_registry())
}

fn assemble(dir: &std::path::Path, edge_url: &str) -> EngineCore {
    assemble_as(dir, edge_url, "device-box")
}

fn assemble_as(dir: &std::path::Path, edge_url: &str, device_id: &str) -> EngineCore {
    std::fs::create_dir_all(dir).expect("create data dir");
    std::fs::write(dir.join("device-id"), device_id).expect("write device id");
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

    let workspace_room = format!("ws4/{ORG}/{USER}");
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

/// The gh#126 exit criterion, over the gh#145 mechanism: a box whose engine is
/// up and roomed never reads offline on another device for longer than one
/// staleness window.
///
/// Two engines (the "box" and a "viewer" — same user, same org, as Brede's Mac
/// and box are) share the fake edge. What must reach the viewer is no longer a
/// heartbeat the box sends: it is the ROOM's answer about who it can see,
/// derived from the socket the box joined on and pushed when the viewer's own
/// `%EPH` join woke the room. That answer must fold into the box's device row
/// `lastSeenAt` — the exact value every sidebar's presence verdict reads.
///
/// Then the edge is redeployed (all sockets die, and with them the room's
/// knowledge of anyone) and presence must come back on its own, because the
/// incident review showed the real outage began exactly here: sockets died, and
/// what failed next determined whether eight amber rows lied for hours. The
/// census must also carry the presence line up the whole way
/// (`workspace_presence`/`org_presence`, the gh#126 EdgeHealth addition).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_roomed_box_reads_fresh_on_another_device_and_survives_a_redeploy() {
    let edge = fake_edge().await;
    let box_dir = tempfile::tempdir().expect("tempdir");
    let viewer_dir = tempfile::tempdir().expect("tempdir");
    let box_core = assemble_as(box_dir.path(), &edge.url, "device-box");
    let viewer = assemble_as(viewer_dir.path(), &edge.url, "device-viewer");

    let devices = viewer.workspace.watch_devices();
    let box_seen_after = |floor: chrono::DateTime<chrono::Utc>| {
        devices
            .borrow()
            .iter()
            .find(|d| d.id == "device-box")
            .and_then(|d| d.last_seen_at)
            .is_some_and(|at| at >= floor)
    };

    // The room's answer must land in the viewer's device row well inside one
    // staleness window.
    let start = chrono::Utc::now();
    wait_until("the viewer to see the box on the room's answer", || {
        box_seen_after(start)
    })
    .await;
    wait_until("the census to carry the presence line", || {
        let health = box_core.edge_health();
        health.workspace_presence == Some(true) && health.org_presence == Some(true)
    })
    .await;

    // 10:06 — the deploy cycles every Durable Object. Presence must recover
    // without anyone restarting anything, exactly like the doc rooms do.
    edge.redeploy();
    let after_deploy = chrono::Utc::now();
    wait_until("presence to flow again after the redeploy", || {
        box_seen_after(after_deploy)
    })
    .await;
    wait_until("the presence census to recover", || {
        box_core.edge_health().workspace_presence == Some(true)
    })
    .await;
}

/// The gh#145 exit criterion, and the one nothing else in this file would
/// notice: an idle fleet must generate NO periodic traffic on the sync rooms.
///
/// Cloudflare notified at 90% of the daily Durable Object duration free tier on
/// a morning with zero agents running. `SessionRoom` had burned 31.2k GB-s over
/// 36.47k requests; `DeviceRoom`, on comparable traffic, 0.386. The difference
/// was not load — it was that every engine wrote `presence/{deviceId} → now`
/// into two `SessionRoom`s every 15s, and a `%EPH` frame is a real message that
/// WAKES the object. One object awake around the clock bills 10,800 GB-s/day:
/// 83% of the tier, permanently, before anyone asks the box to do anything.
///
/// So: watch a quiet fleet for longer than that beat's whole period and assert
/// nothing arrives — while the box goes on reading online throughout, which is
/// the half of the fix that is easy to break. A room that says nothing is not a
/// fleet that has gone away.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_idle_fleet_sends_no_periodic_presence_traffic() {
    let edge = fake_edge().await;
    let box_dir = tempfile::tempdir().expect("tempdir");
    let viewer_dir = tempfile::tempdir().expect("tempdir");
    let _box_core = assemble_as(box_dir.path(), &edge.url, "device-box");
    let viewer = assemble_as(viewer_dir.path(), &edge.url, "device-viewer");

    let devices = viewer.workspace.watch_devices();
    let box_seen_after = |floor: chrono::DateTime<chrono::Utc>| {
        devices
            .borrow()
            .iter()
            .find(|d| d.id == "device-box")
            .and_then(|d| d.last_seen_at)
            .is_some_and(|at| at >= floor)
    };

    let start = chrono::Utc::now();
    wait_until("the viewer to see the box", || box_seen_after(start)).await;

    // Everything that follows is a fleet doing nothing at all.
    let writes_before = edge.client_eph_writes();
    let quiet_start = chrono::Utc::now();
    // Longer than the deleted beat's period, so at least one would have landed.
    tokio::time::sleep(Duration::from_secs(18)).await;

    assert_eq!(
        edge.client_eph_writes(),
        writes_before,
        "an idle fleet wrote presence to the rooms — the 15s beat is back, and \
         with it a Durable Object that can never hibernate"
    );
    assert!(
        box_seen_after(quiet_start),
        "the box stopped reading present during a quiet window — silence from a \
         room is not evidence that anyone left (gh#126)"
    );
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

/// gh#148 — retiring a workspace-room generation (`ws3/` → `ws4/`) is a
/// migration nobody performs.
///
/// A generation bump renames the room, and a room name IS the Durable Object's
/// identity: the new name resolves to a different object with EMPTY storage.
/// Upstream did this because ws3's update log had been left causally broken by
/// the 2026-08-04 abort-thrash and could not be repaired in place — the point
/// of the bump is precisely to walk away from that storage.
///
/// Walking away is only safe because the edge is not authoritative for this
/// doc. Every signed-in device keeps a complete local replica (`workspace2` in
/// its own `DocsStore`), so the virgin room is re-seeded by whoever joins it
/// first and merged into by the rest. Nothing is copied server-side; there is
/// no migration script, no cutover window, and no operator step.
///
/// So assert the property the whole plan rests on: take a device with a
/// populated workspace, delete the room's server-side storage out from under
/// it, and require the content back — with nobody restarting anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abandoning_a_workspace_rooms_storage_is_reseeded_from_the_device() {
    let edge = fake_edge().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let core = assemble(dir.path(), &edge.url);
    let workspace_room = format!("ws4/{ORG}/{USER}");

    core.workspace
        .create_space("space-a", "device-box", "/tmp/space-a", None, false)
        .expect("create space row");
    core.workspace
        .create_chat("chat-a", "space-a", None, None)
        .expect("create chat row");

    wait_until("the workspace to reach the edge at all", || {
        edge.room_chat_ids(&workspace_room) == ["chat-a"]
    })
    .await;

    // The bump. From here the device is talking to storage that has never
    // heard of it — the ws4 object on its very first request.
    edge.abandon_room_storage(&workspace_room);
    assert!(
        edge.room_chat_ids(&workspace_room).is_empty(),
        "the point of a generation bump is that the new room starts empty"
    );

    // Nobody is told to do anything. The rejoin's resubmit-from-version-vector
    // path sees a server that holds nothing, and uploads the whole local doc.
    wait_until("the abandoned room to be re-seeded from the local replica", || {
        edge.room_chat_ids(&workspace_room) == ["chat-a"]
    })
    .await;

    // And the room is live afterwards, not merely re-populated: a chat created
    // after the break syncs the ordinary way.
    core.workspace
        .create_chat("chat-b", "space-a", None, None)
        .expect("create chat row after the break");
    wait_until("the re-seeded room to keep taking writes", || {
        let mut ids = edge.room_chat_ids(&workspace_room);
        ids.sort();
        ids == ["chat-a", "chat-b"]
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
