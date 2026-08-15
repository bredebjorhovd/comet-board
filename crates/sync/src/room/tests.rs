//! Fake-transport unit tests: an in-memory duplex (`mpsc` pipes) to a
//! `FakeEdge` that mirrors `edge/src/session-room.ts` semantics — join with VV
//! backfill, DocUpdate import + Ack + broadcast, fragmentation above the
//! payload budget, and injectable InvalidUpdate acks for the stale-peer path.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use super::*;

const TEST_TIMEOUT: Duration = Duration::from_secs(10);

async fn wait_until(condition: impl FnMut() -> bool) {
    wait_until_within(TEST_TIMEOUT, condition).await;
}

/// [`wait_until`] with its own budget — for waits that legitimately outlast
/// the default one (a backoff ladder sitting at BACKOFF_CAP, say).
async fn wait_until_within(budget: Duration, mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(budget, async {
        loop {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("condition not reached in time");
}

struct FakeConn {
    tx: mpsc::Sender<Vec<u8>>,
    task: tokio::task::JoinHandle<()>,
}

struct FakeEdge {
    doc: LoroDoc,
    eph: EphemeralStore,
    conns: Mutex<Vec<FakeConn>>,
    fragments: Mutex<HashMap<BatchId, FragmentBuffer>>,
    /// When set, the next %LOR DocUpdate is rejected with InvalidUpdate
    /// without being imported (simulates the shallow-trim stale-peer case).
    reject_next_update: AtomicBool,
    /// When set, frames are accepted but never answered — the 2026-07-30
    /// wedged-DO shape: the runtime keeps the socket (and would keep
    /// auto-ponging keepalives) while the room never speaks.
    mute: AtomicBool,
    /// When set, the room answers the `%LOR` join and then dies — the gh#396
    /// shape: a DO hitting its duration cap mid-session, or aborting itself
    /// out of a poisoned WASM instance (gh#378). Every dial "joins".
    die_after_join: AtomicBool,
    leaves: AtomicUsize,
    join_requests: AtomicUsize,
    /// Empty-VV joins ask for a full snapshot. Recovery must cap these at one
    /// per connection episode even if the answer remains incomplete.
    full_snapshot_requests: AtomicUsize,
    /// Optional JoinResponse VV override for validation-failure tests.
    advertised_vv: Mutex<Option<VersionVector>>,
    /// Deterministic window for a test to subscribe before backfill arrives.
    backfill_delay_ms: AtomicU64,
    /// When present, the next join receives this causally-closed prefix plus
    /// RejoinSuggested instead of the ordinary full backfill. Mirrors the
    /// edge's CPU-bounded continuation path (gh#452).
    continuation_backfill: Mutex<Option<Vec<u8>>>,
    /// Persistently advertise undecodable VV bytes after the initial join.
    malformed_versions: AtomicBool,
    /// Swallow this many `%EPH` JoinRequests (consume, answer nothing) — the
    /// gh#126 shape: the doc room joins fine while the presence sub-join is
    /// lost, and nothing at any layer notices.
    swallow_eph_joins: AtomicUsize,
    /// `%EPH` JoinRequests received (swallowed ones included).
    eph_join_requests: AtomicUsize,
    /// Connector dials (initial connect + every redial).
    dials: AtomicUsize,
    /// `%LOR` DocUpdate messages received (liveness probes must send none).
    loro_doc_updates: AtomicUsize,
}

impl FakeEdge {
    fn new() -> Arc<Self> {
        Self::with_doc(LoroDoc::new())
    }

    fn with_doc(doc: LoroDoc) -> Arc<Self> {
        Arc::new(Self {
            doc,
            eph: EphemeralStore::new(30_000),
            conns: Mutex::new(Vec::new()),
            fragments: Mutex::new(HashMap::new()),
            reject_next_update: AtomicBool::new(false),
            mute: AtomicBool::new(false),
            die_after_join: AtomicBool::new(false),
            leaves: AtomicUsize::new(0),
            join_requests: AtomicUsize::new(0),
            full_snapshot_requests: AtomicUsize::new(0),
            advertised_vv: Mutex::new(None),
            backfill_delay_ms: AtomicU64::new(0),
            continuation_backfill: Mutex::new(None),
            malformed_versions: AtomicBool::new(false),
            swallow_eph_joins: AtomicUsize::new(0),
            eph_join_requests: AtomicUsize::new(0),
            dials: AtomicUsize::new(0),
            loro_doc_updates: AtomicUsize::new(0),
        })
    }

    fn connector(self: &Arc<Self>) -> Arc<dyn Connector> {
        Arc::new(FakeConnector { edge: self.clone() })
    }

    /// Kill every live connection (client observes an abrupt close).
    fn kick_all(&self) {
        for conn in self.conns.lock().unwrap().drain(..) {
            conn.task.abort();
            drop(conn.tx);
        }
    }

    /// Forget one connection so its handler can end it by returning: the
    /// registry holds the last sender keeping the client's stream open.
    fn forget(&self, conn: &mpsc::Sender<Vec<u8>>) {
        self.conns
            .lock()
            .unwrap()
            .retain(|live| !live.tx.same_channel(conn));
    }

    async fn reply(&self, to: &mpsc::Sender<Vec<u8>>, message: &ProtocolMessage) {
        let _ = to.send(encode(message).expect("encode")).await;
    }

    /// Mirror of the edge's `sendUpdates`: fragment any single update above
    /// the payload budget.
    async fn send_updates(
        &self,
        to: &mpsc::Sender<Vec<u8>>,
        crdt: CrdtType,
        room_id: &str,
        update: Vec<u8>,
    ) {
        if update.len() <= FRAGMENT_BYTES {
            self.reply(
                to,
                &ProtocolMessage::DocUpdate {
                    crdt,
                    room_id: room_id.to_string(),
                    updates: vec![update],
                    batch_id: new_batch_id(),
                },
            )
            .await;
            return;
        }
        let batch_id = new_batch_id();
        self.reply(
            to,
            &ProtocolMessage::DocUpdateFragmentHeader {
                crdt,
                room_id: room_id.to_string(),
                batch_id,
                fragment_count: update.len().div_ceil(FRAGMENT_BYTES) as u64,
                total_size_bytes: update.len() as u64,
            },
        )
        .await;
        for (index, chunk) in update.chunks(FRAGMENT_BYTES).enumerate() {
            self.reply(
                to,
                &ProtocolMessage::DocUpdateFragment {
                    crdt,
                    room_id: room_id.to_string(),
                    batch_id,
                    index: index as u64,
                    fragment: chunk.to_vec(),
                },
            )
            .await;
        }
    }

    async fn handle(&self, reply_to: &mpsc::Sender<Vec<u8>>, bytes: &[u8]) {
        let message = decode(bytes).expect("client sent an undecodable frame");
        if self.mute.load(Ordering::SeqCst) {
            // Wedged room: consume the frame, answer nothing. Joins are still
            // counted so tests can assert each redial re-attempted the
            // handshake.
            if matches!(
                message,
                ProtocolMessage::JoinRequest {
                    crdt: CrdtType::Loro,
                    ..
                }
            ) {
                self.join_requests.fetch_add(1, Ordering::SeqCst);
            }
            return;
        }
        match message {
            ProtocolMessage::JoinRequest {
                crdt: CrdtType::Loro,
                room_id,
                version,
                ..
            } => {
                self.join_requests.fetch_add(1, Ordering::SeqCst);
                if version.is_empty() {
                    self.full_snapshot_requests.fetch_add(1, Ordering::SeqCst);
                }
                let advertised_version = if self.malformed_versions.load(Ordering::SeqCst) {
                    vec![0xff, 0x00, 0xff]
                } else {
                    self.advertised_vv
                        .lock()
                        .unwrap()
                        .clone()
                        .unwrap_or_else(|| self.doc.oplog_vv())
                        .encode()
                };
                self.reply(
                    reply_to,
                    &ProtocolMessage::JoinResponseOk {
                        crdt: CrdtType::Loro,
                        room_id: room_id.clone(),
                        permission: Permission::Write,
                        version: advertised_version,
                        extra: None,
                    },
                )
                .await;
                let backfill_delay_ms = self.backfill_delay_ms.load(Ordering::SeqCst);
                if backfill_delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(backfill_delay_ms)).await;
                }
                let continuation = self.continuation_backfill.lock().unwrap().take();
                if let Some(prefix) = continuation {
                    self.send_updates(reply_to, CrdtType::Loro, &room_id, prefix)
                        .await;
                    self.reply(
                        reply_to,
                        &ProtocolMessage::RoomError {
                            crdt: CrdtType::Loro,
                            room_id,
                            code: RoomErrorCode::RejoinSuggested,
                            message: "continue backfill".into(),
                        },
                    )
                    .await;
                    return;
                }
                let backfill = if version.is_empty() {
                    self.doc.export(ExportMode::Snapshot)
                } else {
                    match VersionVector::decode(&version) {
                        Ok(vv)
                            if !self.doc.is_shallow()
                                || vv.includes_vv(&self.doc.shallow_since_vv().to_vv()) =>
                        {
                            self.doc.export(ExportMode::updates(&vv))
                        }
                        // Production's stale-peer rule: a client VV behind
                        // the shallow root gets a full snapshot, never a
                        // post-root delta with dependencies it cannot satisfy.
                        Ok(_) | Err(_) => self.doc.export(ExportMode::Snapshot),
                    }
                }
                .expect("export backfill");
                if !backfill.is_empty() {
                    self.send_updates(reply_to, CrdtType::Loro, &room_id, backfill)
                        .await;
                }
            }
            ProtocolMessage::JoinRequest {
                crdt: CrdtType::LoroEphemeralStore,
                room_id,
                ..
            } => {
                self.eph_join_requests.fetch_add(1, Ordering::SeqCst);
                // gh#126: a swallowed presence join — accepted socket, healthy
                // doc room, and this one frame vanishes.
                if self
                    .swallow_eph_joins
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                    .is_ok()
                {
                    return;
                }
                self.reply(
                    reply_to,
                    &ProtocolMessage::JoinResponseOk {
                        crdt: CrdtType::LoroEphemeralStore,
                        room_id: room_id.clone(),
                        permission: Permission::Write,
                        version: Vec::new(),
                        extra: None,
                    },
                )
                .await;
                let all = self.eph.encode_all();
                if !all.is_empty() {
                    self.send_updates(reply_to, CrdtType::LoroEphemeralStore, &room_id, all)
                        .await;
                }
            }
            ProtocolMessage::DocUpdate {
                crdt,
                room_id,
                updates,
                batch_id,
            } => {
                if crdt == CrdtType::Loro {
                    self.loro_doc_updates.fetch_add(1, Ordering::SeqCst);
                }
                self.apply(reply_to, crdt, &room_id, batch_id, updates)
                    .await;
            }
            ProtocolMessage::DocUpdateFragmentHeader {
                crdt,
                batch_id,
                fragment_count,
                total_size_bytes,
                ..
            } => {
                self.fragments.lock().unwrap().insert(
                    batch_id,
                    FragmentBuffer {
                        crdt,
                        parts: vec![None; fragment_count as usize],
                        received: 0,
                        total_size: total_size_bytes as usize,
                    },
                );
            }
            ProtocolMessage::DocUpdateFragment {
                crdt,
                room_id,
                batch_id,
                index,
                fragment,
            } => {
                enum Outcome {
                    /// Header lost (hibernation analogue) — FragmentTimeout.
                    Timeout,
                    Incomplete,
                    Complete(Vec<u8>),
                }
                let outcome = {
                    let mut fragments = self.fragments.lock().unwrap();
                    match fragments.get_mut(&batch_id) {
                        None => Outcome::Timeout,
                        Some(buffer) => {
                            if buffer.parts[index as usize].is_none() {
                                buffer.received += 1;
                            }
                            buffer.parts[index as usize] = Some(fragment);
                            if buffer.received < buffer.parts.len() {
                                Outcome::Incomplete
                            } else {
                                let buffer = fragments.remove(&batch_id).unwrap();
                                let mut total = Vec::with_capacity(buffer.total_size);
                                for part in buffer.parts.into_iter().flatten() {
                                    total.extend_from_slice(&part);
                                }
                                Outcome::Complete(total)
                            }
                        }
                    }
                };
                match outcome {
                    Outcome::Timeout => {
                        self.reply(
                            reply_to,
                            &ProtocolMessage::Ack {
                                crdt,
                                room_id,
                                ref_id: batch_id,
                                status: UpdateStatusCode::FragmentTimeout,
                            },
                        )
                        .await;
                    }
                    Outcome::Incomplete => {}
                    Outcome::Complete(total) => {
                        self.apply(reply_to, crdt, &room_id, batch_id, vec![total])
                            .await;
                    }
                }
            }
            ProtocolMessage::Leave { .. } => {
                self.leaves.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }
    }

    async fn broadcast(&self, message: &ProtocolMessage) {
        let senders: Vec<_> = self
            .conns
            .lock()
            .unwrap()
            .iter()
            .map(|conn| conn.tx.clone())
            .collect();
        for sender in senders {
            self.reply(&sender, message).await;
        }
    }

    /// Mirror of the edge's `applyUpdates`: import, ack, broadcast to peers.
    async fn apply(
        &self,
        reply_to: &mpsc::Sender<Vec<u8>>,
        crdt: CrdtType,
        room_id: &str,
        batch_id: BatchId,
        updates: Vec<Vec<u8>>,
    ) {
        let ack = |status| ProtocolMessage::Ack {
            crdt,
            room_id: room_id.to_string(),
            ref_id: batch_id,
            status,
        };
        if crdt == CrdtType::Loro && self.reject_next_update.swap(false, Ordering::SeqCst) {
            self.reply(reply_to, &ack(UpdateStatusCode::InvalidUpdate))
                .await;
            return;
        }
        let ok = match crdt {
            CrdtType::Loro => updates
                .iter()
                .filter(|u| !u.is_empty())
                .all(|u| self.doc.import(u).is_ok()),
            CrdtType::LoroEphemeralStore => updates
                .iter()
                .filter(|u| !u.is_empty())
                .all(|u| self.eph.apply(u).is_ok()),
            _ => false,
        };
        if !ok {
            self.reply(reply_to, &ack(UpdateStatusCode::InvalidUpdate))
                .await;
            return;
        }
        self.reply(reply_to, &ack(UpdateStatusCode::Ok)).await;
        // Broadcast to every other live connection (edge excludes the sender).
        let peers: Vec<mpsc::Sender<Vec<u8>>> = self
            .conns
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.tx.clone())
            .filter(|tx| !tx.same_channel(reply_to))
            .collect();
        for peer in peers {
            for update in &updates {
                self.send_updates(&peer, crdt, room_id, update.clone())
                    .await;
            }
        }
    }
}

struct FakeConnector {
    edge: Arc<FakeEdge>,
}

impl Connector for FakeConnector {
    fn connect(&self) -> BoxFuture<'static, Result<Pipe, SyncError>> {
        let edge = self.edge.clone();
        Box::pin(async move {
            edge.dials.fetch_add(1, Ordering::SeqCst);
            let (client_tx, mut server_rx) = mpsc::channel::<Vec<u8>>(256);
            let (server_tx, client_rx) = mpsc::channel::<Vec<u8>>(256);
            let reply_to = server_tx.clone();
            let handler_edge = edge.clone();
            let task = tokio::spawn(async move {
                while let Some(bytes) = server_rx.recv().await {
                    handler_edge.handle(&reply_to, &bytes).await;
                    // gh#396: answer the join, then die. Deregistering first
                    // drops the registry's sender; returning drops this one,
                    // and the client's stream ends.
                    if handler_edge.die_after_join.load(Ordering::SeqCst) && is_loro_join(&bytes) {
                        handler_edge.forget(&reply_to);
                        return;
                    }
                }
            });
            edge.conns.lock().unwrap().push(FakeConn {
                tx: server_tx,
                task,
            });
            Ok(Pipe {
                tx: client_tx,
                rx: client_rx,
            })
        })
    }
}

fn is_loro_join(bytes: &[u8]) -> bool {
    matches!(
        decode(bytes),
        Ok(ProtocolMessage::JoinRequest {
            crdt: CrdtType::Loro,
            ..
        })
    )
}

fn doc_text(doc: &LoroDoc) -> String {
    doc.get_text("t").to_string()
}

#[tokio::test]
async fn join_backfills_server_state_into_fresh_doc() {
    let edge = FakeEdge::new();
    edge.doc.get_text("t").insert(0, "server state").unwrap();
    edge.doc.commit();

    let doc = LoroDoc::new();
    let client = RoomClient::connect_with(edge.connector(), "room-1", doc.clone())
        .await
        .expect("connect");
    wait_until(|| doc_text(&doc) == "server state").await;
    client.shutdown().await.unwrap();
}

/// gh#452: a large workspace backfill is delivered over several bounded
/// JoinRequest events on one hibernatable socket. The production phone client
/// already treats RejoinSuggested as "send my newly advanced VV again"; pin
/// that behavior so edge chunking cannot turn into reconnect/backoff churn.
#[tokio::test]
async fn rejoin_suggested_backfill_converges_on_the_same_phone_connection() {
    let server = LoroDoc::new();
    server.set_peer_id(1).unwrap();
    server.get_text("t").insert(0, "first half ").unwrap();
    server.commit();
    let prefix = server
        .export(ExportMode::updates(&VersionVector::default()))
        .unwrap();
    server
        .get_text("t")
        .insert("first half ".len(), "second half")
        .unwrap();
    server.commit();

    let edge = FakeEdge::with_doc(server);
    *edge.continuation_backfill.lock().unwrap() = Some(prefix);
    let doc = LoroDoc::new();
    let client = RoomClient::connect_with(edge.connector(), "room-1", doc.clone())
        .await
        .expect("initial bounded join answers");

    wait_until(|| doc_text(&doc) == doc_text(&edge.doc)).await;
    assert_eq!(
        edge.dials.load(Ordering::SeqCst),
        1,
        "continuation must stay on the hibernatable socket"
    );
    assert!(
        edge.join_requests.load(Ordering::SeqCst) >= 2,
        "the phone must request the remaining prefix with its advanced VV"
    );
    client.shutdown().await.unwrap();
}

/// gh#450: a full old replica cannot merge a newer server snapshot whose
/// history was trimmed past that replica. Loro returns `Ok(ImportStatus)` with
/// pending ranges and leaves the transcript at its old prefix; a room client
/// that treats `Ok(_)` as convergence stalls forever. Recovery must reseed
/// from the validated server snapshot and replay device-local unresolved
/// command intent into the replacement.
#[tokio::test]
async fn stale_full_client_reseeds_from_shallow_server_without_losing_local_command() {
    let server = LoroDoc::new();
    server.set_peer_id(1).unwrap();
    let messages = server.get_list("messages");
    for id in ["m1", "m2", "m3"] {
        messages.insert(messages.len(), id).unwrap();
        server.commit();
    }
    let old_snapshot = server.export(ExportMode::Snapshot).unwrap();

    let client_doc = LoroDoc::new();
    client_doc.import(&old_snapshot).unwrap();
    client_doc.set_peer_id(2).unwrap();
    let stale_session = comet_doc::SessionDoc::from_doc(client_doc.clone());
    let local_command = comet_doc::SessionCommandEntry {
        id: "local-command".into(),
        payload: comet_doc::SessionCommandPayload::Interrupt {},
        context: Vec::new(),
        issued_by: "phone-1".into(),
        issued_at: 1,
        based_on: None,
        expires_at: None,
        status: comet_doc::SessionCommandStatus::Pending,
        resolution: None,
    };
    stale_session.queue_command(&local_command).unwrap();
    stale_session
        .queue_command(&comet_doc::SessionCommandEntry {
            id: "already-applied".into(),
            status: comet_doc::SessionCommandStatus::Applied,
            ..local_command.clone()
        })
        .unwrap();
    stale_session
        .queue_command(&comet_doc::SessionCommandEntry {
            id: "another-device".into(),
            issued_by: "tablet-1".into(),
            ..local_command.clone()
        })
        .unwrap();

    for id in ["m4", "m5"] {
        messages.insert(messages.len(), id).unwrap();
        server.commit();
    }
    let shallow = server
        .export(ExportMode::shallow_snapshot(&server.oplog_frontiers()))
        .unwrap();
    let shallow_server = LoroDoc::new();
    let fresh_status = shallow_server.import(&shallow).unwrap();
    assert!(
        fresh_status.pending.is_none(),
        "fresh reseed must be complete"
    );
    assert!(
        shallow_server.is_shallow(),
        "fixture must trim real history"
    );

    // Pin the exact non-throwing failure signature from the incident.
    let probe = LoroDoc::new();
    probe
        .import(&client_doc.export(ExportMode::Snapshot).unwrap())
        .unwrap();
    let stale_status = probe.import(&shallow).unwrap();
    assert!(
        stale_status.pending.is_some(),
        "shallow snapshot must remain pending in the stale full replica"
    );
    assert_eq!(probe.get_list("messages").len(), 3);

    let edge = FakeEdge::with_doc(shallow_server);
    let replacement_owner = Arc::new(Mutex::new(None));
    let replacement_owner_for_callback = replacement_owner.clone();
    let client = RoomClient::connect_with_recovery(
        edge.connector(),
        "room-1",
        client_doc,
        DocRecovery::for_device(
            "phone-1",
            Arc::new(move |replacement| {
                *replacement_owner_for_callback.lock().unwrap() = Some(replacement);
            }),
        ),
    )
    .await
    .expect("connect");

    wait_until(|| client.doc().get_list("messages").len() == 5).await;
    wait_until(|| edge.doc.get_list("commands").len() == 1).await;
    let replacement = replacement_owner
        .lock()
        .unwrap()
        .clone()
        .expect("application ownership must move to the replacement");
    assert_eq!(replacement.get_list("messages").len(), 5);
    let recovered = comet_doc::SessionDoc::from_doc(client.doc())
        .read_commands()
        .unwrap();
    assert_eq!(recovered, vec![local_command.clone()]);
    let received = comet_doc::SessionDoc::from_doc(edge.doc.clone())
        .read_commands()
        .unwrap();
    assert_eq!(received, vec![local_command]);
    assert_eq!(
        edge.join_requests.load(Ordering::SeqCst),
        1,
        "a server-initiated shallow backfill needs only the normal join"
    );
    assert_eq!(
        edge.full_snapshot_requests.load(Ordering::SeqCst),
        0,
        "a valid shallow snapshot reseeds locally without another DO request"
    );
    client.shutdown().await.unwrap();
}

#[test]
fn public_connect_requires_an_explicit_replacement_owner() {
    let future = RoomClient::connect(
        "ws://127.0.0.1:1",
        "room-public-contract",
        LoroDoc::new(),
        DocRecovery::replacing(Arc::new(|_| {})),
    );
    drop(future); // Signature/ownership contract only; no dial is started.
}

#[tokio::test]
async fn disabled_recovery_never_swaps_away_from_the_supplied_document() {
    let server = LoroDoc::new();
    server.set_peer_id(21).unwrap();
    let messages = server.get_list("messages");
    for id in ["m1", "m2", "m3"] {
        messages.insert(messages.len(), id).unwrap();
        server.commit();
    }
    let stale = LoroDoc::new();
    stale
        .import(&server.export(ExportMode::Snapshot).unwrap())
        .unwrap();
    for id in ["m4", "m5"] {
        messages.insert(messages.len(), id).unwrap();
        server.commit();
    }
    let snapshot = server
        .export(ExportMode::shallow_snapshot(&server.oplog_frontiers()))
        .unwrap();
    let shallow_server = LoroDoc::new();
    shallow_server.import(&snapshot).unwrap();
    let edge = FakeEdge::with_doc(shallow_server);

    let client = RoomClient::connect_with(edge.connector(), "room-1", stale.clone())
        .await
        .expect("normal join");
    wait_until(|| edge.full_snapshot_requests.load(Ordering::SeqCst) == 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(stale.get_list("messages").len(), 3);
    assert_eq!(client.doc().get_list("messages").len(), 3);
    assert_eq!(edge.join_requests.load(Ordering::SeqCst), 2);
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn repeated_pending_imports_request_only_one_recovery_snapshot_and_publish_nothing() {
    let server = LoroDoc::new();
    server.set_peer_id(11).unwrap();
    let messages = server.get_list("messages");
    for id in ["m1", "m2", "m3"] {
        messages.insert(messages.len(), id).unwrap();
        server.commit();
    }
    let old_snapshot = server.export(ExportMode::Snapshot).unwrap();
    let stale_client = LoroDoc::new();
    stale_client.import(&old_snapshot).unwrap();
    for id in ["m4", "m5"] {
        messages.insert(messages.len(), id).unwrap();
        server.commit();
    }
    let shallow = server
        .export(ExportMode::shallow_snapshot(&server.oplog_frontiers()))
        .unwrap();
    let shallow_server = LoroDoc::new();
    assert!(shallow_server.import(&shallow).unwrap().pending.is_none());

    // Advertise one op beyond the snapshot. A fresh import is complete but
    // cannot validate against this VV, so both the normal backfill and the one
    // explicit recovery snapshot remain incomplete from the client's point of
    // view. This pins the bounded failure path, not the happy reseed path.
    let advertised = LoroDoc::new();
    advertised.import(&shallow).unwrap();
    advertised.set_peer_id(99).unwrap();
    advertised.get_map("fixture").insert("ahead", true).unwrap();
    advertised.commit();

    let edge = FakeEdge::with_doc(shallow_server);
    *edge.advertised_vv.lock().unwrap() = Some(advertised.oplog_vv());
    edge.backfill_delay_ms.store(25, Ordering::SeqCst);

    let client = RoomClient::connect_with(edge.connector(), "room-1", stale_client)
        .await
        .expect("normal join answers");
    let mut events = client.events();
    wait_until(|| edge.join_requests.load(Ordering::SeqCst) == 2).await;
    tokio::time::sleep(Duration::from_millis(150)).await;

    assert_eq!(
        edge.join_requests.load(Ordering::SeqCst),
        2,
        "one normal join plus one explicit recovery exchange"
    );
    assert_eq!(
        edge.full_snapshot_requests.load(Ordering::SeqCst),
        1,
        "pending imports must not poll or repeatedly fetch snapshots"
    );
    assert_eq!(
        edge.eph_join_requests.load(Ordering::SeqCst),
        1,
        "the recovery answer must not repeat ordinary presence-join work"
    );
    assert_eq!(
        edge.loro_doc_updates.load(Ordering::SeqCst),
        0,
        "the recovery answer must not upload the stale graph"
    );
    assert_eq!(client.doc().get_list("messages").len(), 3);
    while let Ok(event) = events.try_recv() {
        assert_ne!(
            event,
            RoomEvent::RemoteUpdate,
            "an unvalidated pending import is not convergence"
        );
    }
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn persistent_malformed_server_versions_park_after_one_dial_and_join() {
    let edge = FakeEdge::new();
    let client = RoomClient::connect_with(edge.connector(), "room-1", LoroDoc::new())
        .await
        .expect("initial valid join");
    assert_eq!(edge.dials.load(Ordering::SeqCst), 1);
    assert_eq!(edge.join_requests.load(Ordering::SeqCst), 1);

    edge.malformed_versions.store(true, Ordering::SeqCst);
    edge.broadcast(&ProtocolMessage::JoinResponseOk {
        crdt: CrdtType::Loro,
        room_id: "room-1".into(),
        permission: Permission::Write,
        version: vec![0xff, 0x00, 0xff],
        extra: None,
    })
    .await;
    tokio::time::sleep(Duration::from_millis(750)).await;

    assert_eq!(
        edge.dials.load(Ordering::SeqCst),
        1,
        "malformed protocol state cannot heal by redialing the same DO"
    );
    assert_eq!(
        edge.join_requests.load(Ordering::SeqCst),
        1,
        "a parked client must issue no further room joins"
    );
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn malformed_initial_version_fails_once_without_background_retry() {
    let edge = FakeEdge::new();
    edge.malformed_versions.store(true, Ordering::SeqCst);
    let result = RoomClient::connect_with(edge.connector(), "room-1", LoroDoc::new()).await;
    assert!(matches!(result, Err(SyncError::Protocol(_))));
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(edge.dials.load(Ordering::SeqCst), 1);
    assert_eq!(edge.join_requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn join_pushes_local_history_the_server_lacks() {
    let edge = FakeEdge::new();
    let doc = LoroDoc::new();
    doc.get_text("t").insert(0, "local first").unwrap();
    doc.commit();

    let client = RoomClient::connect_with(edge.connector(), "room-1", doc.clone())
        .await
        .expect("connect");
    wait_until(|| doc_text(&edge.doc) == "local first").await;
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn local_updates_push_ack_and_broadcast_to_peer() {
    let edge = FakeEdge::new();
    let doc_a = LoroDoc::new();
    let doc_b = LoroDoc::new();
    let a = RoomClient::connect_with(edge.connector(), "room-1", doc_a.clone())
        .await
        .expect("connect a");
    let b = RoomClient::connect_with(edge.connector(), "room-1", doc_b.clone())
        .await
        .expect("connect b");

    doc_a.get_text("t").insert(0, "hello from a").unwrap();
    doc_a.commit();

    wait_until(|| doc_text(&edge.doc) == "hello from a").await;
    wait_until(|| doc_text(&doc_b) == "hello from a").await;
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_update_ack_triggers_rejoin_and_resubmit_from_vv() {
    let edge = FakeEdge::new();
    let doc = LoroDoc::new();
    let client = RoomClient::connect_with(edge.connector(), "room-1", doc.clone())
        .await
        .expect("connect");

    let joins_before = edge.join_requests.load(Ordering::SeqCst);
    edge.reject_next_update.store(true, Ordering::SeqCst);
    doc.get_text("t").insert(0, "retried write").unwrap();
    doc.commit();

    // The edge rejected the first submission without importing; the client
    // must rejoin (resync) and resubmit from the server's VV until converged.
    wait_until(|| doc_text(&edge.doc) == "retried write").await;
    assert!(
        edge.join_requests.load(Ordering::SeqCst) > joins_before,
        "client must rejoin"
    );
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn large_updates_fragment_out_and_reassemble_in() {
    let edge = FakeEdge::new();
    let doc_a = LoroDoc::new();
    let doc_b = LoroDoc::new();
    let a = RoomClient::connect_with(edge.connector(), "room-1", doc_a.clone())
        .await
        .expect("connect a");
    let b = RoomClient::connect_with(edge.connector(), "room-1", doc_b.clone())
        .await
        .expect("connect b");

    // Well above FRAGMENT_BYTES: A's push fragments client→server, and the
    // broadcast to B fragments server→client (reassembly both directions).
    let big = "x".repeat(3 * FRAGMENT_BYTES + 12345);
    doc_a.get_text("t").insert(0, &big).unwrap();
    doc_a.commit();

    wait_until(|| doc_text(&edge.doc) == big).await;
    wait_until(|| doc_text(&doc_b) == big).await;
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}

#[tokio::test]
async fn ephemeral_presence_relays_between_peers() {
    let edge = FakeEdge::new();
    let a = RoomClient::connect_with(edge.connector(), "room-1", LoroDoc::new())
        .await
        .expect("connect a");
    let b = RoomClient::connect_with(edge.connector(), "room-1", LoroDoc::new())
        .await
        .expect("connect b");

    a.ephemeral().set("device:a", "online");
    wait_until(|| b.ephemeral().get("device:a") == Some("online".into())).await;

    // Late joiner receives the server's accumulated presence on join.
    let c = RoomClient::connect_with(edge.connector(), "room-1", LoroDoc::new())
        .await
        .expect("connect c");
    wait_until(|| c.ephemeral().get("device:a") == Some("online".into())).await;

    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
    c.shutdown().await.unwrap();
}

#[tokio::test]
async fn reconnects_with_backoff_and_rejoins_after_connection_loss() {
    let edge = FakeEdge::new();
    let doc = LoroDoc::new();
    let client = RoomClient::connect_with(edge.connector(), "room-1", doc.clone())
        .await
        .expect("connect");
    let mut events = client.events();

    edge.kick_all();
    // Write while disconnected: must arrive after the automatic rejoin via the
    // join-time VV diff.
    doc.get_text("t").insert(0, "written offline").unwrap();
    doc.commit();

    wait_until(|| doc_text(&edge.doc) == "written offline").await;

    // Lifecycle events observed: a disconnect, then a (re)connect.
    let mut saw_disconnect = false;
    let mut saw_reconnect = false;
    while let Ok(event) = events.try_recv() {
        match event {
            RoomEvent::Disconnected => saw_disconnect = true,
            RoomEvent::Connected if saw_disconnect => saw_reconnect = true,
            _ => {}
        }
    }
    assert!(
        saw_disconnect && saw_reconnect,
        "expected Disconnected then Connected"
    );
    client.shutdown().await.unwrap();
}

/// gh#116: holding a `RoomClient` is not holding a room. The flag has to track
/// the SESSION — false the moment the socket dies, true again only when a join
/// is answered — because every "is this device online" answer above it (the
/// engine's edge-health census, `comet status`, doctor) is read from here.
/// Before this existed, a supervisor could only ask "do I have a client", and a
/// box whose edge sockets all died answered yes for 25 minutes.
#[tokio::test]
async fn connected_tracks_the_session_not_the_existence_of_a_client() {
    let edge = FakeEdge::new();
    let client = RoomClient::connect_with(edge.connector(), "room-1", LoroDoc::new())
        .await
        .expect("connect");
    assert!(client.connected(), "a joined client reports connected");

    edge.kick_all();
    wait_until(|| !client.connected()).await;

    // The client is untouched — only the socket died — and the redial restores
    // the flag with no help from anyone.
    wait_until(|| client.connected()).await;
    client.shutdown().await.unwrap();
}

/// The supervisor's death certificate: when the actor task ends, its flag drops
/// and the watch CLOSES. That is the only way a layer above can tell "still
/// redialing" from "will never dial again" — the event channel cannot say it,
/// because the client holds a sender of its own and so it never closes.
#[tokio::test]
async fn the_health_watch_closes_when_the_actor_ends() {
    let edge = FakeEdge::new();
    let client = RoomClient::connect_with(edge.connector(), "room-1", LoroDoc::new())
        .await
        .expect("connect");
    let mut health = client.watch_connected();
    assert!(*health.borrow_and_update());

    client.shutdown().await.unwrap();

    // Drains the final `false` (if it has not been observed yet), then errors.
    let closed = tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            if health.changed().await.is_err() {
                return true;
            }
        }
    })
    .await
    .expect("the watch must close, not hang");
    assert!(closed);
    assert!(
        !*health.borrow(),
        "a closed watch must read as disconnected, never as the last live value"
    );
}

#[tokio::test]
async fn shutdown_sends_leave() {
    let edge = FakeEdge::new();
    let client = RoomClient::connect_with(edge.connector(), "room-1", LoroDoc::new())
        .await
        .expect("connect");
    client.shutdown().await.unwrap();
    wait_until(|| edge.leaves.load(Ordering::SeqCst) >= 1).await;
}

/// The per-dial URL provider seam: a signed-out provider fails the connect
/// fast with `SyncError::Auth` (no socket is ever attempted).
#[tokio::test]
async fn connect_via_surfaces_url_provider_auth_error() {
    struct SignedOut;
    impl UrlProvider for SignedOut {
        fn url(&self) -> BoxFuture<'static, Result<String, SyncError>> {
            Box::pin(async { Err(SyncError::Auth("signed out".into())) })
        }
    }
    let result = RoomClient::connect_via(
        Arc::new(SignedOut),
        "room-1",
        LoroDoc::new(),
        DocRecovery::replacing(Arc::new(|_| {})),
    )
    .await;
    match result {
        Ok(_) => panic!("connect must fail"),
        Err(err) => assert!(matches!(err, SyncError::Auth(_)), "got: {err}"),
    }
}

#[tokio::test]
async fn first_connect_failure_is_returned() {
    struct FailingConnector;
    impl Connector for FailingConnector {
        fn connect(&self) -> BoxFuture<'static, Result<Pipe, SyncError>> {
            Box::pin(async { Err(SyncError::WebSocket("refused".into())) })
        }
    }
    let result =
        RoomClient::connect_with(Arc::new(FailingConnector), "room-1", LoroDoc::new()).await;
    match result {
        Ok(_) => panic!("connect must fail"),
        Err(err) => assert!(matches!(err, SyncError::WebSocket(_))),
    }
}

// ── 2026-07-30 sync-stall regressions ───────────────────────────────────────
//
// The workspace DO accepted sockets it never serviced: the runtime auto-ponged
// our keepalives (satisfying the silence lease without waking the DO),
// JoinResponseOk never came, and every engine hung mute for 3+ hours. These
// tests pin the recovery behavior under a paused clock: a mute room and a hung
// dial are redials on backoff, while a healthy-but-quiet (hibernating) room is
// probed and left alone.

/// Initial connect against a mute room must fail within the join deadline —
/// not hang awaiting a JoinResponseOk that will never come.
#[tokio::test(start_paused = true)]
async fn initial_connect_to_mute_room_fails_within_join_deadline() {
    let edge = FakeEdge::new();
    edge.mute.store(true, Ordering::SeqCst);
    let result = tokio::time::timeout(
        JOIN_RESPONSE_DEADLINE + Duration::from_secs(5),
        RoomClient::connect_with(edge.connector(), "room-1", LoroDoc::new()),
    )
    .await
    .expect("connect must give up at the join deadline, not hang");
    match result {
        Ok(_) => panic!("connect must fail against a mute room"),
        Err(err) => assert!(matches!(err, SyncError::WebSocket(_)), "got: {err}"),
    }
    assert!(
        edge.join_requests.load(Ordering::SeqCst) >= 1,
        "the join must actually have been sent"
    );
}

/// The incident shape: an ESTABLISHED client reconnects into a room that keeps
/// the socket open (keepalives still auto-answered) but never answers the
/// join. The client must abandon the socket at the deadline and keep redialing
/// on backoff — the old behavior was an unbounded, log-less hang.
#[tokio::test(start_paused = true)]
async fn established_client_redials_when_rejoin_goes_unanswered() {
    let edge = FakeEdge::new();
    let doc = LoroDoc::new();
    let client = RoomClient::connect_with(edge.connector(), "room-1", doc.clone())
        .await
        .expect("connect");

    let dials_before = edge.dials.load(Ordering::SeqCst);
    let joins_before = edge.join_requests.load(Ordering::SeqCst);
    edge.mute.store(true, Ordering::SeqCst);
    edge.kick_all();

    // The first redial lands right after the kick (backoff base) and sends a
    // join that is never answered; the SECOND redial can only happen if the
    // join deadline abandoned that socket.
    tokio::time::sleep(JOIN_RESPONSE_DEADLINE + Duration::from_secs(5)).await;
    let dials = edge.dials.load(Ordering::SeqCst);
    assert!(
        dials >= dials_before + 2,
        "join deadline must abandon the mute socket within one deadline; saw {} redials",
        dials - dials_before
    );

    // And the cycle keeps going (deadline → backoff → redial), each attempt
    // re-sending a join.
    tokio::time::sleep(Duration::from_secs(120)).await;
    let dials = edge.dials.load(Ordering::SeqCst);
    let joins = edge.join_requests.load(Ordering::SeqCst);
    assert!(
        dials >= dials_before + 4,
        "redial cycle stalled after {} attempts",
        dials - dials_before
    );
    assert!(
        joins >= joins_before + 4,
        "each redial must re-attempt the join handshake"
    );
    drop(client);
}

/// gh#396: a room that JOINS and then dies must climb the backoff ladder like
/// any other failure.
///
/// The DO answers the JoinRequest and the socket dies a moment later — the
/// duration cap reached mid-session, or the `ctx.abort()` of the gh#378
/// WASM-poisoning escalation. Resetting the ladder on the join answer alone
/// made this cycle run at BACKOFF_BASE forever: ~4 dials/second, per room,
/// against an edge that is already unwell, with no ceiling and nothing else in
/// the client capable of that rate.
#[tokio::test(start_paused = true)]
async fn a_room_that_joins_and_then_dies_is_not_redialed_at_base_backoff() {
    let edge = FakeEdge::new();
    let client = RoomClient::connect_with(edge.connector(), "room-1", LoroDoc::new())
        .await
        .expect("connect");

    // From here every dial gets its join answered and then loses the socket.
    edge.die_after_join.store(true, Ordering::SeqCst);
    let before = edge.dials.load(Ordering::SeqCst);
    edge.kick_all();

    // Five minutes of joins-then-deaths. The ladder (jittered, so between
    // half and all of each step) needs ~8 dials to reach BACKOFF_CAP and
    // spends the rest of the window there: a few dozen at the very most. The
    // unfixed loop spent this window at 250ms a dial — 1200 of them.
    tokio::time::sleep(Duration::from_secs(300)).await;
    let dials = edge.dials.load(Ordering::SeqCst) - before;
    assert!(
        dials <= 40,
        "join-then-die must back off; saw {dials} dials in 5 minutes"
    );
    assert!(dials >= 5, "the room must keep trying; saw {dials} dials");

    // And the ceiling holds: once at the cap the rate stays there.
    let at_cap = edge.dials.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(120)).await;
    let dials = edge.dials.load(Ordering::SeqCst) - at_cap;
    assert!(
        dials <= 12,
        "backoff must stay at the cap; saw {dials} dials in 2 minutes"
    );
    drop(client);
}

/// The other half of gh#396: escalation must not punish a room that WORKED.
/// A session that carried the room past `HEALTHY_SESSION` and then lost its
/// socket (an edge deploy, a NAT timeout) reconnects at base backoff — even if
/// the ladder was at the cap when it got there.
#[tokio::test(start_paused = true)]
async fn a_working_session_earns_a_fresh_ladder() {
    let edge = FakeEdge::new();
    let client = RoomClient::connect_with(edge.connector(), "room-1", LoroDoc::new())
        .await
        .expect("connect");

    // Drive the ladder to the cap with join-then-die dials…
    edge.die_after_join.store(true, Ordering::SeqCst);
    edge.kick_all();
    tokio::time::sleep(Duration::from_secs(300)).await;

    // …then let a dial stick, and give it a working session's worth of life.
    edge.die_after_join.store(false, Ordering::SeqCst);
    wait_until_within(Duration::from_secs(120), || client.connected()).await;
    tokio::time::sleep(HEALTHY_SESSION + Duration::from_secs(1)).await;

    edge.kick_all();
    wait_until(|| !client.connected()).await;
    let down_at = tokio::time::Instant::now();
    wait_until_within(Duration::from_secs(120), || client.connected()).await;
    assert!(
        down_at.elapsed() < Duration::from_secs(2),
        "a healthy session must reset the ladder; rejoin took {:?}",
        down_at.elapsed()
    );
    client.shutdown().await.unwrap();
}

/// A dial that never resolves (`provider.url()` or `connect_async` hanging —
/// the `WsConnector` shape) must be cut at CONNECT_TIMEOUT and retried, not
/// wedge the actor forever.
#[tokio::test(start_paused = true)]
async fn hung_dial_times_out_and_redials() {
    struct HangAfterFirst {
        edge: Arc<FakeEdge>,
        dials: AtomicUsize,
    }
    impl Connector for HangAfterFirst {
        fn connect(&self) -> BoxFuture<'static, Result<Pipe, SyncError>> {
            if self.dials.fetch_add(1, Ordering::SeqCst) == 0 {
                self.edge.connector().connect()
            } else {
                Box::pin(std::future::pending())
            }
        }
    }

    let edge = FakeEdge::new();
    let connector = Arc::new(HangAfterFirst {
        edge: edge.clone(),
        dials: AtomicUsize::new(0),
    });
    let client = RoomClient::connect_with(connector.clone(), "room-1", LoroDoc::new())
        .await
        .expect("connect");

    edge.kick_all();
    // Every subsequent dial hangs; each must be cut at CONNECT_TIMEOUT and
    // retried on backoff. A wedged dial would freeze the count at 2.
    tokio::time::sleep(Duration::from_secs(120)).await;
    let dials = connector.dials.load(Ordering::SeqCst);
    assert!(
        dials >= 4,
        "hung dials must time out and be retried; saw {dials}"
    );
    drop(client);
}

/// An initial connect whose dial hangs must fail fast with a WebSocket error
/// instead of hanging the caller.
#[tokio::test(start_paused = true)]
async fn initial_hung_dial_fails_within_connect_timeout() {
    struct HangingConnector;
    impl Connector for HangingConnector {
        fn connect(&self) -> BoxFuture<'static, Result<Pipe, SyncError>> {
            Box::pin(std::future::pending())
        }
    }
    let result = tokio::time::timeout(
        CONNECT_TIMEOUT + Duration::from_secs(5),
        RoomClient::connect_with(Arc::new(HangingConnector), "room-1", LoroDoc::new()),
    )
    .await
    .expect("connect must give up at CONNECT_TIMEOUT, not hang");
    match result {
        Ok(_) => panic!("connect must fail on a hung dial"),
        Err(err) => assert!(matches!(err, SyncError::WebSocket(_)), "got: {err}"),
    }
}

/// The counterpart guard: a healthy-but-QUIET room (a hibernating DO with no
/// doc traffic) must never be treated as dead. The client probes it with an
/// idempotent rejoin after ROOM_PROBE_AFTER; the room answers and the session
/// survives with zero redials.
#[tokio::test(start_paused = true)]
async fn quiet_healthy_room_is_probed_not_killed() {
    let edge = FakeEdge::new();
    let doc = LoroDoc::new();
    let client = RoomClient::connect_with(edge.connector(), "room-1", doc.clone())
        .await
        .expect("connect");
    let mut events = client.events();

    // Seed real history first: an empty local doc short-circuits the
    // resubmit block before the includes_vv gate, which would mask a
    // regression where probes upload no-op updates (round-2 review finding).
    doc.get_text("t").insert(0, "seed").unwrap();
    doc.commit();
    wait_until(|| doc_text(&edge.doc) == "seed").await;

    let joins_before = edge.join_requests.load(Ordering::SeqCst);
    let updates_before = edge.loro_doc_updates.load(Ordering::SeqCst);

    // Two hours of total silence — several probe intervals (the backoff
    // doubles per quiet probe: 15m, +30m, +60m).
    tokio::time::sleep(Duration::from_secs(2 * 60 * 60)).await;

    assert_eq!(
        edge.dials.load(Ordering::SeqCst),
        1,
        "a quiet healthy room must not be redialed"
    );
    assert!(
        edge.join_requests.load(Ordering::SeqCst) > joins_before,
        "liveness probes must have fired during the quiet stretch"
    );
    // Probes must be pure reads: a probe that uploads even a no-op DocUpdate
    // dirties the room's caches and re-arms its daily alarm — the idle fleet
    // would never actually go idle (adversarial-review finding).
    assert_eq!(
        edge.loro_doc_updates.load(Ordering::SeqCst),
        updates_before,
        "liveness probes must not upload DocUpdates"
    );
    while let Ok(event) = events.try_recv() {
        assert_ne!(
            event,
            RoomEvent::Disconnected,
            "quiet healthy room was spuriously dropped"
        );
    }

    // The session is still fully live after the quiet stretch.
    doc.get_text("t").insert(0, "still alive").unwrap();
    doc.commit();
    wait_until(|| doc_text(&edge.doc) == "still aliveseed").await;
    client.shutdown().await.unwrap();
}

/// The most important regression guard: the normal join/backfill/reconnect
/// cycle still works with the incident fixes in place.
#[tokio::test]
async fn healthy_session_join_backfill_reconnect_still_works() {
    let edge = FakeEdge::new();
    edge.doc.get_text("t").insert(0, "server state").unwrap();
    edge.doc.commit();

    let doc = LoroDoc::new();
    let client = RoomClient::connect_with(edge.connector(), "room-1", doc.clone())
        .await
        .expect("initial connect");
    // Backfill must complete.
    wait_until(|| doc_text(&doc) == "server state").await;

    // Local write must push and arrive on the server.
    doc.get_text("t").insert(0, "client adds").unwrap();
    doc.commit();
    wait_until(|| doc_text(&edge.doc).contains("client adds")).await;

    // Simulate a reconnect: kick all live connections.
    let mut events = client.events();
    edge.kick_all();

    // The client must detect the close, reconnect with backoff, and converge.
    wait_until(|| matches!(events.try_recv(), Ok(RoomEvent::Disconnected))).await;
    wait_until(|| matches!(events.try_recv(), Ok(RoomEvent::Connected))).await;
    assert_eq!(doc_text(&doc), doc_text(&edge.doc), "docs must converge");

    // One more write to verify the re-joined session is fully functional.
    doc.get_text("t").insert(0, "after reconnect").unwrap();
    doc.commit();
    wait_until(|| doc_text(&edge.doc).contains("after reconnect")).await;

    client.shutdown().await.unwrap();
}

/// gh#126 — the presence sub-join is no longer fire-and-forget.
///
/// The doc room joins fine while the one `%EPH` JoinRequest vanishes (a
/// swallowed frame, or a JoinError that used to only warn). Before the fix,
/// `joined_eph` stayed false for the session's whole life: every outbound
/// heartbeat was silently dropped while doc sync looked perfect — a box that
/// is up, roomed, and "offline" on every other device. The session now
/// re-sends the join on a cadence until it lands.
#[tokio::test(start_paused = true)]
async fn a_swallowed_presence_join_is_retried_until_presence_flows() {
    let edge = FakeEdge::new();
    edge.swallow_eph_joins.store(2, Ordering::SeqCst);

    let client = RoomClient::connect_with(edge.connector(), "room-eph", LoroDoc::new())
        .await
        .expect("connect");
    assert!(client.connected(), "doc room is up");
    assert!(
        !client.presence_joined(),
        "the eph join was swallowed — presence must KNOW it is down"
    );

    // A heartbeat published while presence is down…
    client.ephemeral().set("presence/dev-a", 1_i64);

    // …must still reach the room: the retry re-sends the join (twice, here)
    // and the join answer re-uploads the full local presence state. The
    // retries live at virtual t=15s/30s — beyond `wait_until`'s 10s budget —
    // so this wait carries its own, wider one.
    tokio::time::timeout(Duration::from_secs(120), async {
        while !matches!(
            edge.eph.get("presence/dev-a"),
            Some(loro::LoroValue::I64(1))
        ) || !client.presence_joined()
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("presence must recover through the join retry");
    assert!(
        edge.eph_join_requests.load(Ordering::SeqCst) >= 3,
        "two swallowed + at least one answered"
    );
    client.shutdown().await.unwrap();
}
