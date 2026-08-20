//! `RoomClient` — a loro-protocol room client over WebSocket, speaking to the
//! TS edge's SessionRoom Durable Object (`edge/src/session-room.ts`).
//!
//! Wire format (loro-protocol 0.3, identical bytes to the npm package the edge
//! imports): every frame is `4-byte CRDT magic ("%LOR"/"%EPH"/…), varbytes
//! roomId, 1-byte message type, payload`. The messages this client exchanges:
//!
//! - `JoinRequest {auth, version}` → `JoinResponseOk {permission, version}` /
//!   `JoinError {code, message}` — version bytes are Loro `VersionVector`
//!   encodings; the server backfills `export({mode:"update", from: clientVV})`
//!   or a full snapshot when the client VV is empty/garbled.
//! - `DocUpdate {updates[], batchId}` acknowledged by `Ack {refId, status}`.
//! - `DocUpdateFragmentHeader {batchId, fragmentCount, totalSizeBytes}` +
//!   `DocUpdateFragment {batchId, index, fragment}` for payloads above the
//!   256KB message cap (the edge fragments at 200_000 payload bytes).
//! - `RoomError {RejoinSuggested | Evicted}`, `Leave`.
//!
//! Sync discipline (mirrors the edge's expectations):
//! - On (re)join, the server's `JoinResponseOk.version` is used to export and
//!   push everything the server lacks — this doubles as resend-after-reconnect
//!   (unacked local commits are re-derived from the doc, never queued).
//! - `Ack{InvalidUpdate}` is the §3.1 stale-peer signal (import concurrent to
//!   a shallow-snapshot trim): the client rejoins on the same socket to resync
//!   fresh, then re-submits from the server's VV.
//! - `Ack{FragmentTimeout}` (reassembly state lost to DO hibernation): the
//!   whole batch is resent.
//! - Presence rides the `%EPH` sub-room as `loro::awareness::EphemeralStore`
//!   payloads relayed verbatim.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use crate::convergence::{
    ConvergenceJournal, ConvergenceRecovery, ConvergenceState, RecoveryPhase, UNACKNOWLEDGED_ALERT,
};
use futures::future::BoxFuture;
use futures::{SinkExt, StreamExt};
use loro::awareness::EphemeralStore;
use loro::{ExportMode, LoroDoc, VersionVector};
use loro_protocol::{
    BatchId, CrdtType, JoinErrorCode, Permission, ProtocolMessage, RoomErrorCode, UpdateStatusCode,
    decode, encode,
};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// Payload bytes per outbound fragment — mirrors the edge's `FRAGMENT_BYTES`
/// (leaves envelope room under loro-protocol's 256KB message cap).
const FRAGMENT_BYTES: usize = 200_000;
/// Refuse absurd inbound fragment batches (a healthy backfill snapshot is MBs).
const MAX_REASSEMBLED_BYTES: usize = 256 * 1024 * 1024;
const MAX_FRAGMENT_COUNT: u64 = 16 * 1024;
/// Presence timeout, matching the edge's `new EphemeralStore(30_000)`.
const EPHEMERAL_TIMEOUT_MS: i64 = 30_000;
/// Resend cadence for the `%EPH` sub-room join while the doc room is up and
/// presence is not (gh#126). The eph join used to be fire-and-forget: sent once
/// per session after `JoinResponseOk`, a `JoinError` only warned, and an
/// unanswered join left `joined_eph` false forever — every outbound heartbeat
/// silently dropped while doc sync stayed perfectly healthy, which is exactly
/// the shape of a box that is up, roomed, and "offline" on every other screen.
/// Nothing polices presence liveness the way `JOIN_RESPONSE_DEADLINE` polices
/// the doc join, so the join is simply re-sent until it lands.
const EPH_JOIN_RETRY: Duration = Duration::from_secs(15);
/// Text `"ping"` keepalive interval — answered by the DO's hibernation-safe
/// auto-response pair without waking it. 15s for the same reason as the
/// device relay's (crates/rpc/src/device_room.rs): an idle-flow reaper on a
/// laptop's uplink can fire inside a minute, and a 30s keepalive races it.
const PING_INTERVAL: Duration = Duration::from_secs(15);
/// Silence lease (TRANSPORT level): every ping elicits an auto-pong, so a
/// healthy socket sees inbound traffic at least once per `PING_INTERVAL`. No
/// inbound frame for a couple of intervals plus grace = the socket is dead
/// (half-open TCP after a NAT timeout or sleep/wake) — drop it and let the
/// reconnect loop take over instead of waiting minutes for a TCP write
/// failure. Auto-pongs satisfy this lease ON PURPOSE: they are real proof the
/// TCP path works. What they are NOT is proof the room works — the CF runtime
/// answers them without ever waking the DO — so room-level liveness is
/// enforced separately (`JOIN_RESPONSE_DEADLINE` / `ROOM_PROBE_AFTER` below).
const SILENCE_LEASE: Duration = Duration::from_secs(40);
/// Bound on one whole dial, enforced around `Connector::connect` in
/// `RoomActor::run` so it covers every connector. For the production
/// `WsConnector` both `provider.url()` (a token-endpoint HTTP call) and
/// `connect_async` (a blackholed SYN on a dead uplink) can hang for minutes
/// on their own, wedging the actor with no session, no error, and no log
/// line. Expiry maps to `SyncError::WebSocket` and the normal backoff redial.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// INCIDENT (2026-07-30): the workspace DO (`ws3/{orgId}/{userId}`) refused
/// upgrades for ~3 minutes, then accepted sockets whose JoinRequests it never
/// processed. The ping→pong auto-response kept resetting the silence lease —
/// pongs prove only that Cloudflare is up, never that the room is — and with
/// `joined_lor` false nothing was ever pushed, so no binary frame arrived to
/// betray the wedge either. All four engines sat on dead-but-healthy-looking
/// sockets for 3+ hours with ZERO log lines; recovery took a manual engine
/// restart. The two constants below make a mute room a redial, never a hang.
///
/// Every `%LOR` JoinRequest we send (initial join, stale-peer rejoin,
/// liveness probe) must be answered within this window or the session ends
/// `Lost` and the backoff loop redials. Armed only while a join is actually
/// in flight — an established session with no join outstanding is never
/// killed by it. Generous vs. the sub-second happy path because a cold DO
/// replays its whole update log before answering.
const JOIN_RESPONSE_DEADLINE: Duration = Duration::from_secs(15);
/// Established sessions: after this long without a single %LOR frame from
/// the room, rejoin on the same socket as a liveness probe. Only %LOR frames
/// count: the edge's %EPH path never touches the doc machinery (ensureEph
/// only), so presence acks/broadcasts keep flowing every ~15s from a
/// doc-wedged DO — counting them made this probe unreachable on exactly the
/// room that wedged (adversarial-review finding, round 2). Rejoin is the
/// probe because it is already idempotent (the stale-peer and RejoinSuggested
/// paths rejoin mid-session today), it forces a `JoinResponseOk` out of any
/// healthy room, and its backfill diff is empty when we are in sync. A
/// hibernating-but-healthy DO is simply woken by the probe and answers —
/// hibernation is NOT treated as death, which is why this runs in minutes
/// while the transport lease runs in seconds. The alternative (a server-push
/// heartbeat from the DO) was rejected: emitting one needs a permanent
/// short-interval alarm, i.e. abolishing hibernation for every room in the
/// fleet to detect a failure only clients can act on anyway.
///
/// COST: each probe briefly wakes (and cold-materializes) the DO, so this is
/// the hibernation duty-cycle knob. 15 min × N quiet clients keeps the room
/// asleep >97% of an idle night; text-ping keepalives stay free (runtime
/// auto-response, no wake). Detection latency for the rare mid-session wedge
/// is probe interval + JOIN_RESPONSE_DEADLINE, and the durable replay-crash
/// counter on the edge (session-room.ts ensureDoc) does the actual healing
/// once redials start — the probe only needs to notice, not race. This is
/// the BASE interval; consecutive quiet probes double it (see
/// ROOM_PROBE_MAX) so dormant rooms decay to a handful of wakes a day.
const ROOM_PROBE_AFTER: Duration = Duration::from_secs(900);
/// Probe backoff cap. Every RoomClient probes — including the per-chat
/// clients the engine keeps alive for every chat ever opened and never
/// evicts — so a fixed 15-min cadence would wake (and cold-materialize)
/// every dormant chat DO ~100×/day forever (adversarial-review finding).
/// Doubling per quiet probe up to 4h makes a dormant room cost ~6 wakes/day;
/// any real room traffic resets the cadence to ROOM_PROBE_AFTER.
const ROOM_PROBE_MAX: Duration = Duration::from_secs(4 * 3600);
/// Frames arriving this soon after a probe are the probe's own reply
/// (JoinResponseOk + backfill envelope), not organic traffic — they must not
/// reset the probe backoff or every probe would reset its own decay.
const PROBE_REPLY_GRACE: Duration = Duration::from_secs(30);
const BACKOFF_BASE: Duration = Duration::from_millis(250);
const BACKOFF_CAP: Duration = Duration::from_secs(30);
/// A session must stay joined at least this long for its end to count as a
/// working session and reset the backoff ladder (gh#396).
///
/// Reaching `JoinResponseOk` is NOT that proof. A DO that accepts the join and
/// then dies — the duration cap hit mid-session, the `ctx.abort()` in the
/// WASM-poisoning escalation (gh#378) — answered the join every time, so
/// resetting on `joined` alone made the room redial at BACKOFF_BASE forever:
/// ~4 dials/second, per room, with no ceiling, precisely while the edge is
/// least able to take it. Requiring a lifetime instead bounds the steady state
/// of any join-then-die loop to roughly one dial per session lifetime, and the
/// ladder still climbs to BACKOFF_CAP when the deaths stay fast. Matches
/// `HOST_HEALTHY_SESSION` in the device-room relay, which draws the same line.
const HEALTHY_SESSION: Duration = Duration::from_secs(30);
/// Window over which a post-wake redial is spread across the box's rooms
/// (gh#396). Wake is broadcast to every room actor in the same instant, so an
/// undelayed redial is N simultaneous dials at an edge that has just watched
/// the whole fleet resume. Short enough that recovery is still immediate to a
/// human — the alternative, waiting out a silence lease, is the minute-long
/// stall this wake path exists to kill.
const WAKE_SPREAD: Duration = Duration::from_millis(1000);
/// Stop resubmitting after this many InvalidUpdate-triggered rejoins in one
/// session — our history predates the room's shallow start and can never
/// import; recovery is an app-layer concern (§3.1).
const MAX_INVALID_REJOINS: u32 = 3;
/// How often an established session recomputes its convergence state (gh#483).
///
/// The outbox is written by the document's owner, not by this actor, so the
/// count of unacknowledged local content changes without any frame arriving.
/// A poll is what makes "live but not converged" observable in seconds instead
/// of at the next room event — the incident's whole failure was that nothing
/// ever looked. Cheap by construction: an indexed count over a table that is
/// empty on a converged room.
const CONVERGENCE_POLL: Duration = Duration::from_secs(15);

/// Errors surfaced by [`RoomClient`].
#[derive(Debug, Clone, thiserror::Error)]
pub enum SyncError {
    #[error("websocket: {0}")]
    WebSocket(String),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("join refused: {0}")]
    JoinRefused(String),
    #[error("loro: {0}")]
    Loro(String),
    #[error("auth: {0}")]
    Auth(String),
    #[error("client is shut down")]
    Closed,
}

impl SyncError {
    /// Protocol bytes are deterministic server state, not a transport outage:
    /// redialing the same room cannot repair them and would wake its Durable
    /// Object forever. Callers supervising an initial join must park too.
    pub fn is_permanent(&self) -> bool {
        matches!(self, Self::Protocol(_))
    }
}

/// Per-dial WebSocket URL provider — consulted before EVERY connection attempt,
/// including background reconnects, so a short-lived auth token embedded in the
/// URL (`?token=…`) is re-read fresh rather than frozen at first connect.
/// Return [`SyncError::Auth`] when no valid credential is available (signed
/// out); the reconnect loop backs off and retries.
pub trait UrlProvider: Send + Sync + 'static {
    fn url(&self) -> BoxFuture<'static, Result<String, SyncError>>;
}

/// Fixed URL (dev bearers and tests — tokens that never expire).
pub struct StaticUrl(pub String);

impl UrlProvider for StaticUrl {
    fn url(&self) -> BoxFuture<'static, Result<String, SyncError>> {
        let url = self.0.clone();
        Box::pin(async move { Ok(url) })
    }
}

/// Connection/sync lifecycle notifications (best-effort broadcast; receivers
/// may lag and miss intermediate events).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomEvent {
    /// Joined (or re-joined) the room; backfill and resubmission are underway.
    Connected,
    /// The connection dropped; the client is backing off before reconnecting.
    Disconnected,
    /// Remote loro updates were imported into the doc.
    RemoteUpdate,
    /// Remote ephemeral (presence) state was applied.
    EphemeralUpdate,
    /// The server evicted us; the client will NOT reconnect.
    Evicted,
    /// Malformed protocol state parked the client; no background redial will
    /// occur until an owner explicitly constructs/starts another client.
    ProtocolParked,
    /// The room's CONTENT state changed — converged, pending, recovering, or
    /// blocked. Emitted independently of [`Self::Connected`] on purpose: the
    /// gh#483 incident is a room that is connected and never converges, and a
    /// surface that infers one from the other cannot show it.
    ConvergenceChanged,
}

/// App-owned state that must move with a room client when a shallow server
/// snapshot cannot merge into its stale local document.
///
/// The room validates the replacement, hands it to [`crate::convergence`] —
/// which quarantines the stale document and replays every locally committed
/// semantic entry onto the replacement — and then invokes `on_reseed` before
/// publishing [`RoomEvent::RemoteUpdate`]. Session rooms also name the local
/// device (provenance for the command ledger); workspace/registry rooms use
/// [`DocRecovery::replacing`] because they have no command ledger.
///
/// `on_reseed` is the ownership boundary, not a notification. It must move all
/// app-owned references synchronously. If command creation can retain the old
/// document, a device callback must also gate that creation and reconcile the
/// final old-document command delta before publishing the replacement. The
/// room cannot close a race in an owner it does not control.
///
/// Recovery is ALWAYS convergence-driven: without [`Self::with_journal`] the
/// outbox and quarantine live in a [`MemoryJournal`], so replay and the state
/// reporting behave identically and only the surviving-a-restart part is
/// missing. Anything that persists a document (the engine, the phone) is
/// expected to pass a durable journal.
#[derive(Clone)]
pub struct DocRecovery {
    local_device_id: Option<String>,
    on_reseed: Option<Arc<dyn Fn(LoroDoc) + Send + Sync>>,
    mutation_gate: Option<Arc<std::sync::Mutex<()>>>,
    convergence: Option<ConvergenceRecovery>,
}

impl DocRecovery {
    /// Install session-document recovery with a synchronous replacement owner.
    /// See [`DocRecovery`] for the callback's atomicity contract.
    pub fn for_device(
        device_id: impl Into<String>,
        on_reseed: Arc<dyn Fn(LoroDoc) + Send + Sync>,
    ) -> Self {
        Self {
            local_device_id: Some(device_id.into()),
            on_reseed: Some(on_reseed),
            mutation_gate: None,
            convergence: None,
        }
    }

    /// Install recovery for a document without a device command ledger.
    pub fn replacing(on_reseed: Arc<dyn Fn(LoroDoc) + Send + Sync>) -> Self {
        Self {
            local_device_id: None,
            on_reseed: Some(on_reseed),
            mutation_gate: None,
            convergence: None,
        }
    }

    /// Serialize remote imports and the complete reseed handoff with an
    /// owner's local mutations. The owner and room actor must share this gate.
    pub fn with_mutation_gate(mut self, gate: Arc<std::sync::Mutex<()>>) -> Self {
        self.mutation_gate = Some(gate);
        self
    }

    /// Back this room's outbox and quarantine with a durable journal
    /// (`DocsStore` in the engine). `doc_id` keys the rows — the chat id for a
    /// session room, the stable local doc id for a workspace room.
    pub fn with_journal(
        mut self,
        journal: Arc<dyn ConvergenceJournal>,
        doc_id: impl Into<String>,
    ) -> Self {
        self.convergence = Some(ConvergenceRecovery::new(journal, doc_id));
        self
    }

    /// The recovery driver this room will use, defaulting to a process-local
    /// one so behaviour never depends on whether a store was wired.
    fn convergence(&self, room_id: &str) -> ConvergenceRecovery {
        self.convergence.clone().unwrap_or_else(|| {
            ConvergenceRecovery::new(Arc::new(crate::convergence::MemoryJournal::new()), room_id)
        })
    }

    #[cfg(test)]
    fn disabled() -> Self {
        Self {
            local_device_id: None,
            on_reseed: None,
            mutation_gate: None,
            convergence: None,
        }
    }
}

/// A byte-frame duplex to the room: `tx` outbound, `rx` inbound. Closing
/// either side ends the session.
pub(crate) struct Pipe {
    pub(crate) tx: mpsc::Sender<Vec<u8>>,
    pub(crate) rx: mpsc::Receiver<Vec<u8>>,
}

/// Dials one connection attempt. The production impl speaks WebSocket; tests
/// substitute an in-memory duplex.
pub(crate) trait Connector: Send + Sync + 'static {
    fn connect(&self) -> BoxFuture<'static, Result<Pipe, SyncError>>;
}

struct WsConnector {
    url: Arc<dyn UrlProvider>,
}

impl Connector for WsConnector {
    fn connect(&self) -> BoxFuture<'static, Result<Pipe, SyncError>> {
        let provider = self.url.clone();
        Box::pin(async move {
            // Fresh URL (and therefore fresh `?token=`) on every attempt — an
            // expired access token is never reused across a reconnect. Both
            // this fetch and the handshake below can hang; the actor bounds
            // the whole dial with CONNECT_TIMEOUT.
            let url = provider.url().await?;
            let (ws, _) = tokio_tungstenite::connect_async(&url)
                .await
                .map_err(|e| SyncError::WebSocket(e.to_string()))?;
            let (out_tx, out_rx) = mpsc::channel(64);
            let (in_tx, in_rx) = mpsc::channel(64);
            tokio::spawn(pump(ws, out_rx, in_tx));
            Ok(Pipe {
                tx: out_tx,
                rx: in_rx,
            })
        })
    }
}

/// Shuttle frames between the WebSocket and the actor's channels, plus the
/// text-ping keepalive. Ends (dropping `in_tx`, which the actor observes) when
/// either side closes.
async fn pump(
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    mut out_rx: mpsc::Receiver<Vec<u8>>,
    in_tx: mpsc::Sender<Vec<u8>>,
) {
    let (mut sink, mut stream) = ws.split();
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await; // consume the immediate first tick
    let mut last_rx = tokio::time::Instant::now();
    loop {
        tokio::select! {
            frame = out_rx.recv() => match frame {
                Some(bytes) => {
                    if sink.send(WsMessage::Binary(bytes)).await.is_err() {
                        break;
                    }
                }
                None => {
                    // Actor is done (shutdown): close politely.
                    let _ = sink.send(WsMessage::Close(None)).await;
                    break;
                }
            },
            frame = stream.next() => match frame {
                Some(Ok(WsMessage::Binary(bytes))) => {
                    last_rx = tokio::time::Instant::now();
                    if in_tx.send(bytes).await.is_err() {
                        break;
                    }
                }
                Some(Ok(_)) => {
                    // Text "pong" / control frames: proof of life for the
                    // TRANSPORT lease only. The CF runtime auto-answers our
                    // ping without waking the DO, so this says nothing about
                    // the room (2026-07-30 — see JOIN_RESPONSE_DEADLINE);
                    // room-level liveness is judged in `run_session`, which
                    // only ever sees the binary frames forwarded below.
                    last_rx = tokio::time::Instant::now();
                }
                Some(Err(_)) | None => break,
            },
            _ = ping.tick() => {
                if sink.send(WsMessage::Text("ping".into())).await.is_err() {
                    break;
                }
            }
            _ = tokio::time::sleep_until(last_rx + SILENCE_LEASE) => {
                tracing::warn!("room socket silent past lease; treating as dead");
                break;
            }
        }
    }
}

/// Live-connection flag shared by the actor and its [`RoomClient`].
///
/// The `RoomEvent` broadcast says what just happened and is lossy by design
/// (receivers may lag); this says what is true right now, and a supervisor can
/// poll or await it. Two things make it more than a convenience:
///
/// - it is the difference between "we hold a `RoomClient`" and "we hold a room"
///   — the state gh#116 went dark in, where every slot was `Some(client)` and
///   not one socket was alive;
/// - dropping it (which the actor does on ANY exit, including a panic) closes
///   the watch, so `changed()` erroring is the actor's death certificate. A
///   supervisor cannot get that from the event channel: the `RoomClient` holds
///   a sender of its own, so events never close while the client is alive.
struct ConnectedFlag(watch::Sender<bool>);

impl ConnectedFlag {
    fn set(&self, connected: bool) {
        self.0.send_replace(connected);
    }
}

impl Drop for ConnectedFlag {
    fn drop(&mut self) {
        // Publish the truth before the channel closes, so a watcher that reads
        // `borrow()` after the actor is gone sees "not connected" rather than
        // the last live value.
        self.0.send_replace(false);
    }
}

/// A live room membership for one Loro doc.
///
/// Owns a background task that keeps `doc` converged with the room: pushes
/// local commits (via `subscribe_local_update`), imports remote updates and
/// backfill, relays `%EPH` presence, reassembles/produces fragments, and
/// reconnects with exponential backoff after connection loss. Dropping the
/// client aborts the task immediately; [`RoomClient::shutdown`] leaves the
/// room cleanly first.
pub struct RoomClient {
    doc: Arc<RwLock<LoroDoc>>,
    eph: EphemeralStore,
    events: broadcast::Sender<RoomEvent>,
    connected: watch::Receiver<bool>,
    convergence: watch::Receiver<ConvergenceState>,
    presence: watch::Receiver<bool>,
    shutdown: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<()>>,
    /// Doc + ephemeral local-update subscriptions (drop = unsubscribe).
    _subs: Vec<loro::Subscription>,
    /// Swapped by the actor together with `doc` during a reseed, so local
    /// commits on the replacement keep flowing without rebuilding the client.
    _doc_sub: Arc<Mutex<Option<loro::Subscription>>>,
}

impl RoomClient {
    /// Connect to a loro-protocol room and keep `doc` in sync with it.
    ///
    /// `url` is the full, already-authenticated WebSocket URL (the edge takes
    /// the bearer as `?token=`, e.g. `wss://…/session/{chatId}/ws?token=…`);
    /// `room_id` is the doc room name carried inside the protocol frames (the
    /// chatId, or `ws/{orgId}` for workspace docs).
    ///
    /// Resolves once the initial join handshake succeeds — the JoinRequest
    /// carries the doc's version vector, and the server's backfill (updates or
    /// a full snapshot) is imported as it arrives. A first-attempt failure
    /// (unreachable edge, `JoinError`) is returned as `Err`; only after a
    /// successful join does the client keep reconnecting in the background.
    /// `recovery` is mandatory because a shallow-history repair replaces the
    /// LoroDoc object; its callback must move every app-owned reference to the
    /// supplied replacement. There is deliberately no no-op default.
    pub async fn connect(
        url: &str,
        room_id: &str,
        doc: LoroDoc,
        recovery: DocRecovery,
    ) -> Result<Self, SyncError> {
        Self::connect_via(Arc::new(StaticUrl(url.to_string())), room_id, doc, recovery).await
    }

    /// Like [`Self::connect`], but the WebSocket URL is re-fetched from
    /// `provider` before every dial (initial and reconnects) — the seam for
    /// expiring bearer tokens carried as `?token=`.
    /// Ownership recovery remains explicit for the same reason as `connect`.
    pub async fn connect_via(
        provider: Arc<dyn UrlProvider>,
        room_id: &str,
        doc: LoroDoc,
        recovery: DocRecovery,
    ) -> Result<Self, SyncError> {
        let connector = Arc::new(WsConnector { url: provider });
        Self::connect_with_recovery(connector, room_id, doc, recovery).await
    }

    #[cfg(test)]
    pub(crate) async fn connect_with(
        connector: Arc<dyn Connector>,
        room_id: &str,
        doc: LoroDoc,
    ) -> Result<Self, SyncError> {
        Self::connect_with_recovery(connector, room_id, doc, DocRecovery::disabled()).await
    }

    pub(crate) async fn connect_with_recovery(
        connector: Arc<dyn Connector>,
        room_id: &str,
        doc: LoroDoc,
        recovery: DocRecovery,
    ) -> Result<Self, SyncError> {
        let eph = EphemeralStore::new(EPHEMERAL_TIMEOUT_MS);

        let (local_tx, local_rx) = mpsc::unbounded_channel();
        let doc_generation = Arc::new(AtomicU64::new(0));
        let initial_local_tx = local_tx.clone();
        let initial_generation = doc_generation.load(Ordering::SeqCst);
        let sub_doc = doc.subscribe_local_update(Box::new(move |bytes: &Vec<u8>| {
            let _ = initial_local_tx.send((initial_generation, bytes.clone()));
            true
        }));
        let doc_sub = Arc::new(Mutex::new(Some(sub_doc)));
        let current_doc = Arc::new(RwLock::new(doc.clone()));
        let (eph_tx, eph_rx) = mpsc::unbounded_channel();
        let sub_eph = eph.subscribe_local_updates(Box::new(move |bytes: &Vec<u8>| {
            let _ = eph_tx.send(bytes.clone());
            true
        }));

        let (events, _) = broadcast::channel(256);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (connected_tx, connected_rx) = watch::channel(false);
        let (presence_tx, presence_rx) = watch::channel(false);
        let (convergence_tx, convergence_rx) = watch::channel(ConvergenceState::Converged);
        let (ready_tx, ready_rx) = oneshot::channel();

        // Crash resume, before the first dial: a recovery interrupted between
        // quarantine and acknowledgement finishes here, replaying the
        // quarantined content into whatever document this client was handed.
        // Idempotent by stable id, so a resume that has nothing to do costs one
        // journal lookup (gh#483 §6).
        let convergence = recovery.convergence(room_id);
        match convergence.resume(&doc) {
            Ok(Some(report)) if !report.is_empty() => tracing::warn!(
                room = %room_id,
                replayed = report.total(),
                "replayed an interrupted recovery's quarantined content at open"
            ),
            Ok(_) => {}
            Err(err) => tracing::error!(
                room = %room_id, error = %err,
                "could not resume an interrupted recovery; the quarantine is retained"
            ),
        }

        let actor = RoomActor {
            doc: current_doc.clone(),
            local_tx,
            doc_generation,
            doc_sub: doc_sub.clone(),
            recovery,
            convergence,
            convergence_tx: Arc::new(convergence_tx),
            eph: eph.clone(),
            room_id: room_id.to_string(),
            connector,
            local_rx,
            eph_rx,
            events: events.clone(),
            connected: Arc::new(ConnectedFlag(connected_tx)),
            presence: Arc::new(ConnectedFlag(presence_tx)),
            shutdown: shutdown_rx,
        };
        let task = tokio::spawn(actor.run(ready_tx));

        match ready_rx.await {
            Ok(Ok(())) => Ok(Self {
                doc: current_doc,
                eph,
                events,
                connected: connected_rx,
                convergence: convergence_rx,
                presence: presence_rx,
                shutdown: shutdown_tx,
                task: Some(task),
                _subs: vec![sub_eph],
                _doc_sub: doc_sub,
            }),
            Ok(Err(err)) => {
                task.abort();
                Err(err)
            }
            Err(_) => {
                task.abort();
                Err(SyncError::Closed)
            }
        }
    }

    /// The current synced doc handle. This is the original passed to
    /// `connect` until shallow-history recovery atomically swaps in a validated
    /// replacement.
    pub fn doc(&self) -> LoroDoc {
        self.doc
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Presence store relayed through the room's `%EPH` channel: `set` keys
    /// here to publish, read/subscribe to observe remote peers.
    pub fn ephemeral(&self) -> &EphemeralStore {
        &self.eph
    }

    /// Subscribe to connection/sync lifecycle events.
    pub fn events(&self) -> broadcast::Receiver<RoomEvent> {
        self.events.subscribe()
    }

    /// Is this client joined to the room RIGHT NOW?
    ///
    /// Holding a `RoomClient` is not the same as holding a room: the actor
    /// keeps redialing across drops, and between a lost socket and the next
    /// `JoinResponseOk` this reads false. Every "am I online" answer the engine
    /// gives — `comet status`, doctor, the device overlay — has to come from
    /// here rather than from the existence of the client (gh#116).
    pub fn connected(&self) -> bool {
        *self.connected.borrow()
    }

    /// Is the `%EPH` presence sub-room joined RIGHT NOW?
    ///
    /// Strictly narrower than [`Self::connected`]: presence rides its own
    /// sub-room join on the same socket, and the edge answers (or refuses) it
    /// independently of the doc machinery. A doc-live room with dead presence
    /// is the gh#126 shape — every heartbeat silently dropped while sync looks
    /// perfect — so health surfaces read this, never infer it from
    /// `connected()`.
    pub fn presence_joined(&self) -> bool {
        *self.presence.borrow()
    }

    /// Watch [`Self::presence_joined`] (closes with the actor, like
    /// [`Self::watch_connected`]).
    pub fn watch_presence(&self) -> watch::Receiver<bool> {
        self.presence.clone()
    }

    /// What this room can honestly say about its CONTENT right now (gh#483).
    ///
    /// Strictly independent of [`Self::connected`]. A room can be joined,
    /// ponging, presence-live and still hold 74 transcript entries the edge has
    /// never taken — that is the incident, and it reads
    /// [`ConvergenceState::Pending`] here while `connected()` reads true.
    /// Anything that renders "synced" must read this.
    pub fn convergence(&self) -> ConvergenceState {
        self.convergence.borrow().clone()
    }

    /// Watch [`Self::convergence`]. Closes with the actor, like
    /// [`Self::watch_connected`].
    pub fn watch_convergence(&self) -> watch::Receiver<ConvergenceState> {
        self.convergence.clone()
    }

    /// Watch [`Self::connected`]. The channel CLOSES when the actor task ends
    /// for any reason (clean shutdown, or a panic that would otherwise leave a
    /// client that can never reconnect), so `changed()` returning `Err` is the
    /// supervisor's cue to rebuild this client rather than wait forever.
    pub fn watch_connected(&self) -> watch::Receiver<bool> {
        self.connected.clone()
    }

    /// Dial `room_id` as a FRESH, independent client and report which of
    /// `expected` stable ids it can retrieve — the second half of the gh#483
    /// invariant.
    ///
    /// This is deliberately not a check the recovering client can do on itself:
    /// "my document contains it" and "anyone else asking this room gets it" are
    /// different claims, and only the second one lets the quarantine go. The
    /// client starts from an EMPTY document, so everything it sees came from
    /// the room's own backfill.
    ///
    /// Returns as soon as every expected id is present, or when `budget`
    /// expires — in which case the ids it did see are returned, and the caller
    /// keeps its quarantine.
    pub async fn observe_stable_ids(
        provider: Arc<dyn UrlProvider>,
        room_id: &str,
        expected: &[String],
        budget: Duration,
    ) -> Result<Vec<String>, SyncError> {
        let doc = LoroDoc::new();
        let client = Self::connect_via(
            provider,
            room_id,
            doc.clone(),
            DocRecovery::replacing(Arc::new(|_| {})),
        )
        .await?;
        let observed = Self::await_stable_ids(&doc, expected, budget).await;
        let _ = client.shutdown().await;
        Ok(observed)
    }

    #[cfg(test)]
    pub(crate) async fn observe_stable_ids_with(
        connector: Arc<dyn Connector>,
        room_id: &str,
        expected: &[String],
        budget: Duration,
    ) -> Result<Vec<String>, SyncError> {
        let doc = LoroDoc::new();
        let client = Self::connect_with_recovery(
            connector,
            room_id,
            doc.clone(),
            DocRecovery::replacing(Arc::new(|_| {})),
        )
        .await?;
        let observed = Self::await_stable_ids(&doc, expected, budget).await;
        let _ = client.shutdown().await;
        Ok(observed)
    }

    async fn await_stable_ids(doc: &LoroDoc, expected: &[String], budget: Duration) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + budget;
        let wanted: HashSet<&str> = expected.iter().map(String::as_str).collect();
        loop {
            let observed = crate::convergence::stable_ids(doc).unwrap_or_default();
            let seen: HashSet<&str> = observed.iter().map(String::as_str).collect();
            if wanted.iter().all(|id| seen.contains(id)) {
                return observed;
            }
            if tokio::time::Instant::now() >= deadline {
                return observed;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Leave the room (protocol `Leave` frames + close handshake) and stop the
    /// background task.
    pub async fn shutdown(mut self) -> Result<(), SyncError> {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            let abort = task.abort_handle();
            if tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .is_err()
            {
                abort.abort();
            }
        }
        Ok(())
    }
}

impl Drop for RoomClient {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

// ── background actor ────────────────────────────────────────────────────────

struct RoomActor {
    doc: Arc<RwLock<LoroDoc>>,
    local_tx: mpsc::UnboundedSender<(u64, Vec<u8>)>,
    doc_generation: Arc<AtomicU64>,
    doc_sub: Arc<Mutex<Option<loro::Subscription>>>,
    recovery: DocRecovery,
    /// The convergence driver for this room's document: outbox, quarantine,
    /// replay, and acknowledgement accounting (gh#483).
    convergence: ConvergenceRecovery,
    /// Truthful content state, published for [`RoomClient::convergence`].
    convergence_tx: Arc<watch::Sender<ConvergenceState>>,
    eph: EphemeralStore,
    room_id: String,
    connector: Arc<dyn Connector>,
    local_rx: mpsc::UnboundedReceiver<(u64, Vec<u8>)>,
    eph_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    events: broadcast::Sender<RoomEvent>,
    /// Live-connection truth for the client (and its supervisor). Held as an
    /// `Arc` so the per-connection `Session` can raise it the moment the join
    /// is answered; dropped with the actor, which closes the watch.
    connected: Arc<ConnectedFlag>,
    /// Same discipline for the `%EPH` sub-room: raised on its join answer,
    /// lowered when the session ends — the truth behind
    /// [`RoomClient::presence_joined`].
    presence: Arc<ConnectedFlag>,
    shutdown: watch::Receiver<bool>,
}

enum SessionEnd {
    /// Clean shutdown requested; Leave was sent.
    Shutdown,
    /// Fatal refusal (JoinError / RoomError::Evicted) — do not reconnect.
    Evicted(String),
    /// Connection failed or dropped — reconnect with backoff.
    Lost(SyncError),
}

/// One session as the redial loop needs to see it: how it ended, and the two
/// facts that set the next delay.
struct SessionOutcome {
    end: SessionEnd,
    /// The session stayed joined for at least [`HEALTHY_SESSION`] — proof the
    /// room actually worked, and the only thing that resets the backoff ladder
    /// (gh#396).
    healthy: bool,
    /// The session was ended by a system wake, not by a failure: the redial is
    /// immediate but spread over [`WAKE_SPREAD`], because every room on the box
    /// got the same broadcast.
    woke: bool,
}

impl SessionOutcome {
    /// A session that never ran (dial error, dial timeout) or one that ended
    /// without earning anything.
    fn failed(end: SessionEnd) -> Self {
        Self {
            end,
            healthy: false,
            woke: false,
        }
    }
}

impl RoomActor {
    async fn run(mut self, ready: oneshot::Sender<Result<(), SyncError>>) {
        let mut ready = Some(ready);
        let mut backoff = BACKOFF_BASE;
        // System wake is an EVENT: it ends the (half-open) session immediately
        // and cancels any pending backoff, so the room is redialing a second or
        // so after the lid opens (WAKE_SPREAD, which staggers the box's rooms)
        // instead of waiting out a silence lease.
        let mut wake = crate::wake::subscribe();
        loop {
            if *self.shutdown.borrow() {
                return;
            }
            let dial = tokio::time::timeout(CONNECT_TIMEOUT, self.connector.connect()).await;
            let outcome = match dial {
                Ok(Ok(pipe)) => self.run_session(pipe, &mut wake, &mut ready).await,
                Ok(Err(err)) => SessionOutcome::failed(SessionEnd::Lost(err)),
                Err(_) => {
                    // The dial itself hung (URL provider stall, blackholed
                    // handshake) — without this bound the actor wedged here
                    // forever with no log line (see CONNECT_TIMEOUT).
                    tracing::warn!(room = %self.room_id, timeout = ?CONNECT_TIMEOUT, "dial timed out; backing off to redial");
                    SessionOutcome::failed(SessionEnd::Lost(SyncError::WebSocket(
                        "dial timeout".into(),
                    )))
                }
            };
            // A session that did real work earns a fresh ladder; one that only
            // got a join answer before dying does not (gh#396, HEALTHY_SESSION).
            // Applied BEFORE the match so an eviction's deliberate long
            // backoff below is the last word — reset-after-match let a
            // long-lived session that was then evicted rejoin at 250ms, which
            // is the one case the eviction backoff exists to prevent.
            if outcome.healthy {
                backoff = BACKOFF_BASE;
            }
            match outcome.end {
                SessionEnd::Shutdown => {
                    // `local_rx`/`eph_rx` closing reaches here too — the doc or
                    // ephemeral subscription is gone, so this actor can never
                    // publish another local commit. Say so: before gh#116 it
                    // returned in silence while the `RoomClient` lived on
                    // looking connected, which is precisely the shape of a box
                    // that is up and invisible. The dropped `connected` flag
                    // tells the supervisor to rebuild.
                    if !*self.shutdown.borrow() {
                        tracing::error!(
                            room = %self.room_id,
                            "room actor lost its local-update channel; the room needs rebuilding"
                        );
                    }
                    return;
                }
                SessionEnd::Evicted(reason) => {
                    if let Some(tx) = ready.take() {
                        let _ = tx.send(Err(SyncError::JoinRefused(reason)));
                        return;
                    }
                    // NOT terminal for an established client: a transient
                    // join refusal (expired token racing a refresh, an edge
                    // deploy, a DO handover) used to kill the room FOREVER —
                    // presence went "offline" and stayed there until an app
                    // restart while the per-chat rooms kept working (user
                    // report). Rejoin on a long, capped backoff instead; a
                    // genuinely revoked session just keeps refusing quietly.
                    tracing::warn!(room = %self.room_id, %reason, "evicted from room; rejoining with long backoff");
                    let _ = self.events.send(RoomEvent::Evicted);
                    backoff = BACKOFF_CAP;
                }
                SessionEnd::Lost(err) => {
                    if err.is_permanent() {
                        if let Some(tx) = ready.take() {
                            let _ = tx.send(Err(err));
                            return;
                        }
                        tracing::error!(room = %self.room_id, error = %err,
                            "malformed room protocol; parking without redial");
                        let _ = self.events.send(RoomEvent::ProtocolParked);
                        // Keep the actor—and therefore its terminal state—alive
                        // until shutdown. A supervisor sees ProtocolParked and
                        // drops the client instead of mistaking actor death for
                        // a transient failure worth rebuilding.
                        while !*self.shutdown.borrow() {
                            if self.shutdown.changed().await.is_err() {
                                break;
                            }
                        }
                        return;
                    }
                    if let Some(tx) = ready.take() {
                        // Never joined: fail `connect()` fast instead of
                        // silently retrying in the background.
                        let _ = tx.send(Err(err));
                        return;
                    }
                    tracing::warn!(room = %self.room_id, error = %err, "room connection lost");
                    let _ = self.events.send(RoomEvent::Disconnected);
                }
            }
            // A wake cancels the backoff — during it, or as the thing that
            // ended the session a moment ago.
            let mut woke = outcome.woke;
            if !woke {
                // The ladder value is a ceiling, not a schedule: sleeping
                // exactly `backoff` keeps every room that failed together
                // (an edge deploy, one flaky uplink) redialing together
                // forever. Wait a jittered [backoff/2, backoff) instead —
                // half a step of spread at every rung, widening with the
                // rung, which is where the rooms are most piled up (gh#396).
                let wait = backoff / 2 + crate::jitter::spread(backoff / 2);
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = wake.recv() => woke = true,
                    _ = self.shutdown.changed() => return,
                }
            }
            if woke {
                // Redial NOW with fresh credentials — but every room on this
                // box got that same broadcast in the same millisecond, so
                // stagger the herd across WAKE_SPREAD first (gh#396).
                tokio::select! {
                    _ = tokio::time::sleep(crate::jitter::spread(WAKE_SPREAD)) => {}
                    _ = self.shutdown.changed() => return,
                }
                backoff = BACKOFF_BASE;
                continue;
            }
            backoff = (backoff * 2).min(BACKOFF_CAP);
        }
    }

    /// Drive one connection until it ends. Returns the end reason plus what
    /// the redial loop needs to time the next dial (see [`SessionOutcome`]).
    async fn run_session(
        &mut self,
        mut pipe: Pipe,
        wake: &mut broadcast::Receiver<()>,
        ready: &mut Option<oneshot::Sender<Result<(), SyncError>>>,
    ) -> SessionOutcome {
        // Local updates queued while disconnected are already in the doc; the
        // VV diff pushed on join re-derives them, so stale queue entries are
        // dropped rather than replayed.
        while self.local_rx.try_recv().is_ok() {}
        while self.eph_rx.try_recv().is_ok() {}

        let mut sess = Session {
            doc: self
                .doc
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            current_doc: self.doc.clone(),
            local_tx: self.local_tx.clone(),
            doc_generation: self.doc_generation.clone(),
            doc_sub: self.doc_sub.clone(),
            recovery: self.recovery.clone(),
            convergence: self.convergence.clone(),
            convergence_tx: self.convergence_tx.clone(),
            events_for_state: self.events.clone(),
            acked_vv: VersionVector::default(),
            unacked_since: None,
            stall_reported: false,
            blocked: None,
            eph: self.eph.clone(),
            room_id: self.room_id.clone(),
            tx: pipe.tx.clone(),
            events: self.events.clone(),
            connected: self.connected.clone(),
            presence: self.presence.clone(),
            pending: HashMap::new(),
            fragments: HashMap::new(),
            joined_lor: false,
            joined_at: None,
            joined_eph: false,
            eph_join_sent_at: None,
            invalid_rejoins: 0,
            full_resync_requested: false,
            server_vv: None,
            join_sent_at: None,
            join_is_probe: false,
            last_lor_rx: tokio::time::Instant::now(),
        };

        sess.publish_convergence();
        let version = sess.local_version_bytes();
        if let Err(err) = sess.send_join_loro(version).await {
            return SessionOutcome::failed(SessionEnd::Lost(err));
        }

        let mut convergence_poll = tokio::time::interval(CONVERGENCE_POLL);
        convergence_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        convergence_poll.tick().await; // consume the immediate first tick
        let mut probe_interval = ROOM_PROBE_AFTER;
        let mut last_probe_at: Option<tokio::time::Instant> = None;
        let mut woke = false;

        let end = loop {
            // Two-tier room liveness: an in-flight %LOR JoinRequest has a hard
            // answer deadline; otherwise a long-quiet room gets probed. The
            // deadline runs from the LATER of join-sent and last %LOR frame:
            // a rejoin queued behind a large outbound backlog (slow uplink)
            // keeps eliciting %LOR acks/backfill while it drains, and those
            // prove the DOC machinery works — killing on pure arm-time
            // redialed healthy sessions mid-push (adversarial-review
            // finding). Only %LOR frames extend it (`sess.last_lor_rx`, see
            // that field): %EPH presence keeps flowing from a doc-wedged DO,
            // and letting it extend the deadline turned the hard deadline
            // back into an unbounded hang (round-2 finding). The incident
            // case (zero frames ever) is unchanged.
            let (liveness_at, join_outstanding) = match sess.join_sent_at {
                Some(sent) => (sent.max(sess.last_lor_rx) + JOIN_RESPONSE_DEADLINE, true),
                None => (sess.last_lor_rx + probe_interval, false),
            };
            // Presence must not die quieter than the doc room (gh#126): while
            // the doc join has landed and the `%EPH` join has not — refused
            // with a JoinError, or simply never answered — re-send it on a
            // steady cadence. Cheap (one frame), idempotent server-side, and
            // the only escape from a session whose presence sub-join was
            // swallowed once and would otherwise stay dead for the session's
            // whole life.
            let eph_retry_at = (sess.joined_lor && !sess.joined_eph).then(|| {
                sess.eph_join_sent_at
                    .map_or_else(tokio::time::Instant::now, |at| at + EPH_JOIN_RETRY)
            });
            tokio::select! {
                // Biased so a buffered answer frame always beats an expired
                // deadline in the same poll — never kill with the
                // JoinResponseOk already readable.
                biased;
                // Post-suspend the socket is almost certainly half-open (NAT
                // state gone); ending the session redials immediately with a
                // freshly-provided URL/token instead of waiting out the
                // silence lease. A false positive costs one cheap rejoin.
                _ = wake.recv() => {
                    woke = true;
                    break SessionEnd::Lost(SyncError::WebSocket(
                        "system woke from suspend; reconnecting".into(),
                    ));
                }
                _ = self.shutdown.changed() => {
                    let _ = sess
                        .send(&ProtocolMessage::Leave {
                            crdt: CrdtType::Loro,
                            room_id: sess.room_id.clone(),
                        })
                        .await;
                    if sess.joined_eph {
                        let _ = sess
                            .send(&ProtocolMessage::Leave {
                                crdt: CrdtType::LoroEphemeralStore,
                                room_id: sess.room_id.clone(),
                            })
                            .await;
                    }
                    break SessionEnd::Shutdown;
                }
                frame = pipe.rx.recv() => match frame {
                    None => break SessionEnd::Lost(SyncError::WebSocket("connection closed".into())),
                    Some(bytes) => {
                        let lor_before = sess.last_lor_rx;
                        match sess.handle_frame(&bytes, ready).await {
                            Ok(None) => {}
                            Ok(Some(end)) => break end,
                            Err(err) => break SessionEnd::Lost(err),
                        }
                        // Organic %LOR traffic resets the probe cadence;
                        // frames in a probe's own wake do not (see
                        // PROBE_REPLY_GRACE), and %EPH frames never do.
                        if sess.last_lor_rx > lor_before
                            && last_probe_at.is_none_or(|at| at.elapsed() > PROBE_REPLY_GRACE)
                        {
                            probe_interval = ROOM_PROBE_AFTER;
                        }
                    }
                },
                update = self.local_rx.recv() => match update {
                    None => break SessionEnd::Shutdown, // client dropped
                    // When not yet joined: covered by the join-time VV diff.
                    Some((generation, update)) => {
                        if generation == sess.doc_generation.load(Ordering::SeqCst)
                            && sess.joined_lor
                            && let Err(err) = sess.send_loro_updates(vec![update]).await
                        {
                            break SessionEnd::Lost(err);
                        }
                    }
                },
                update = self.eph_rx.recv() => match update {
                    None => break SessionEnd::Shutdown,
                    // When not yet joined: presence is ephemeral; dropped by design.
                    Some(update) => {
                        if sess.joined_eph
                            && let Err(err) = sess.send_eph_updates(vec![update]).await
                        {
                            break SessionEnd::Lost(err);
                        }
                    }
                },
                _ = async {
                    match eph_retry_at {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    tracing::debug!(room = %self.room_id, "presence sub-room unjoined; re-sending %EPH join");
                    if let Err(err) = sess.send_join_eph().await {
                        break SessionEnd::Lost(err);
                    }
                }
                // The document's owner writes the outbox, not this actor, so
                // "is this room converged?" has to be asked rather than waited
                // for (gh#483 §4/§7). Nothing here touches the socket.
                _ = convergence_poll.tick() => {
                    sess.publish_convergence();
                }
                _ = tokio::time::sleep_until(liveness_at) => {
                    if join_outstanding {
                        // The 2026-07-30 hang: a room that accepted the socket
                        // but never answered the join. Kill the session so the
                        // backoff loop redials — one fresh join re-instantiates
                        // a wedged DO.
                        tracing::warn!(
                            room = %self.room_id,
                            established = sess.joined_lor,
                            deadline = ?JOIN_RESPONSE_DEADLINE,
                            "no JoinResponseOk within deadline; room presumed wedged, redialing"
                        );
                        break SessionEnd::Lost(SyncError::WebSocket(
                            "join deadline expired: room never answered JoinRequest".into(),
                        ));
                    }
                    // Quiet room: send the rejoin probe. A send failure means
                    // the pipe is gone; the answer is policed by the deadline
                    // armed above on the next loop iteration.
                    tracing::debug!(room = %self.room_id, quiet = ?probe_interval, "no room traffic; rejoining as liveness probe");
                    let version = sess.local_version_bytes();
                    if let Err(err) = sess.send_join_loro(version).await {
                        break SessionEnd::Lost(err);
                    }
                    sess.join_is_probe = true;
                    last_probe_at = Some(tokio::time::Instant::now());
                    probe_interval = (probe_interval * 2).min(ROOM_PROBE_MAX);
                }
            }
        };
        // Joined-and-then-died is not a working session (gh#396): the ladder
        // only resets for one that carried the room past HEALTHY_SESSION.
        let healthy = sess
            .joined_at
            .is_some_and(|at| at.elapsed() >= HEALTHY_SESSION);
        // The session is over whatever the reason; nothing is live again until
        // the next JoinResponseOk raises these.
        self.connected.set(false);
        self.presence.set(false);
        SessionOutcome { end, healthy, woke }
    }
}

// ── per-connection protocol session ─────────────────────────────────────────

struct FragmentBuffer {
    crdt: CrdtType,
    parts: Vec<Option<Vec<u8>>>,
    received: usize,
    total_size: usize,
}

struct Session {
    doc: LoroDoc,
    current_doc: Arc<RwLock<LoroDoc>>,
    local_tx: mpsc::UnboundedSender<(u64, Vec<u8>)>,
    doc_generation: Arc<AtomicU64>,
    doc_sub: Arc<Mutex<Option<loro::Subscription>>>,
    recovery: DocRecovery,
    convergence: ConvergenceRecovery,
    convergence_tx: Arc<watch::Sender<ConvergenceState>>,
    /// Broadcast handle used only to announce a content-state change; the
    /// lifecycle events go through `events`.
    events_for_state: broadcast::Sender<RoomEvent>,
    /// Everything this session has proof the edge holds: the version it
    /// advertised on join, merged with the end version of every batch it
    /// acked. This is what retires outbox rows — never the mere fact that a
    /// frame was sent.
    acked_vv: VersionVector,
    /// When local content first became unacknowledged in this session, and
    /// whether the alert threshold has already been reported (once per
    /// session, so a stuck room is a line in the log rather than a stream).
    unacked_since: Option<tokio::time::Instant>,
    stall_reported: bool,
    /// Set when the edge will not take this device's content and recovery
    /// cannot repair it — the room is live and the document is local-only.
    blocked: Option<String>,
    eph: EphemeralStore,
    room_id: String,
    tx: mpsc::Sender<Vec<u8>>,
    events: broadcast::Sender<RoomEvent>,
    /// Raised on every answered `%LOR` join (probe answers included — an
    /// answered probe IS proof the room is live); lowered by the actor when the
    /// session ends.
    connected: Arc<ConnectedFlag>,
    /// Raised on the answered `%EPH` join; lowered with the session. The
    /// health census reads this ([`RoomClient::presence_joined`], gh#126).
    presence: Arc<ConnectedFlag>,
    /// Sent-but-unacked outbound batches, kept for FragmentTimeout resends.
    pending: HashMap<BatchId, Vec<Vec<u8>>>,
    /// Inbound reassembly buffers.
    fragments: HashMap<BatchId, FragmentBuffer>,
    joined_lor: bool,
    /// Instant the FIRST `%LOR` join of this session was answered (a rejoin or
    /// probe answer does not restart it). The session's lifetime as a joined
    /// room, which is what earns a backoff reset — see [`HEALTHY_SESSION`].
    joined_at: Option<tokio::time::Instant>,
    joined_eph: bool,
    /// Instant of the last `%EPH` JoinRequest — `run_session` re-sends on
    /// [`EPH_JOIN_RETRY`] while `joined_lor && !joined_eph` (gh#126).
    eph_join_sent_at: Option<tokio::time::Instant>,
    invalid_rejoins: u32,
    full_resync_requested: bool,
    /// Version advertised by the latest answered join. A fresh import is a
    /// valid server reseed only when it exactly reaches this vector.
    server_vv: Option<VersionVector>,
    /// Instant of the last `%LOR` JoinRequest still awaiting `JoinResponseOk`
    /// (initial join, stale-peer rejoin, or liveness probe); `None` once
    /// answered. `run_session` enforces `JOIN_RESPONSE_DEADLINE` on it.
    join_sent_at: Option<tokio::time::Instant>,
    /// True while the outstanding join is a liveness/recovery probe on an
    /// established session — its answer must not replay join side effects
    /// (%EPH rejoin, Connected re-broadcast, or stale doc upload).
    join_is_probe: bool,
    /// Instant of the last inbound `%LOR` frame — the room-liveness clock
    /// feeding both the join deadline and the probe timer. %EPH frames are
    /// deliberately EXCLUDED: the edge's presence path never touches the doc
    /// machinery, so eph acks/broadcasts keep arriving every ~15s from a
    /// doc-wedged DO, and counting them silenced the probe and pinned the
    /// join deadline open on exactly the room that wedged on 2026-07-30
    /// (adversarial-review finding, round 2). Auto-pongs never reach this
    /// layer at all (pump forwards only binary frames).
    last_lor_rx: tokio::time::Instant,
}

impl Session {
    /// Recompute and publish [`ConvergenceState`] (gh#483 §4).
    ///
    /// Deliberately derived from three durable facts and nothing about the
    /// socket: how much unacknowledged semantic content the outbox holds,
    /// whether a recovery is open, and whether the edge has refused this
    /// device's history. A live socket contributes nothing to it.
    fn publish_convergence(&mut self) {
        let unacked = self.convergence.pending();
        if unacked == 0 {
            self.unacked_since = None;
            self.stall_reported = false;
        } else if self.unacked_since.is_none() {
            self.unacked_since = Some(tokio::time::Instant::now());
        }
        let stalled = self
            .unacked_since
            .is_some_and(|since| since.elapsed() >= UNACKNOWLEDGED_ALERT);
        let state = if let Some(reason) = &self.blocked {
            ConvergenceState::BlockedLocalOnly {
                unacked,
                reason: reason.clone(),
            }
        } else if let Some(record) = self
            .convergence
            .open_recovery()
            .filter(|record| record.phase < RecoveryPhase::Acknowledged)
        {
            ConvergenceState::Recovering {
                phase: record.phase,
                unacked,
            }
        } else if unacked > 0 {
            ConvergenceState::Pending { unacked, stalled }
        } else {
            ConvergenceState::Converged
        };
        // The diagnostic gh#483 §7 asks for: a room that is JOINED while its
        // uploads stay unacknowledged past the threshold. Reported once per
        // session, at error level, naming the count — the state a viewport
        // renders is the same value, so the log and the UI cannot disagree.
        if stalled && !self.stall_reported && self.joined_lor {
            self.stall_reported = true;
            tracing::error!(
                room = %self.room_id,
                unacked,
                threshold_s = UNACKNOWLEDGED_ALERT.as_secs(),
                state = %state.label(),
                "room is joined but local content has been unacknowledged past the \
                 threshold; transport liveness is NOT convergence"
            );
        }
        if *self.convergence_tx.borrow() != state {
            self.convergence_tx.send_replace(state);
            let _ = self.events_for_state.send(RoomEvent::ConvergenceChanged);
        }
    }

    /// Fold new proof of what the edge holds into `acked_vv` and retire the
    /// outbox rows it covers.
    fn note_acknowledged(&mut self, version: &VersionVector) {
        self.acked_vv.merge(version);
        let acked = self.acked_vv.clone();
        if let Err(err) = self.convergence.acknowledge(&acked) {
            tracing::warn!(room = %self.room_id, error = %err,
                "could not retire acknowledged outbox rows");
        }
        self.publish_convergence();
    }

    /// The version an acked batch carried. Derived from the blob itself rather
    /// than from what we believe we exported: the batch may have been built
    /// before a reseed, and only the bytes know what is actually in it.
    fn batch_version(updates: &[Vec<u8>]) -> Option<VersionVector> {
        let mut merged = VersionVector::default();
        let mut any = false;
        for update in updates {
            match LoroDoc::decode_import_blob_meta(update, false) {
                Ok(meta) => {
                    merged.merge(&meta.partial_end_vv);
                    any = true;
                }
                Err(_) => return None,
            }
        }
        any.then_some(merged)
    }

    fn local_version_bytes(&self) -> Vec<u8> {
        let vv = self.doc.oplog_vv();
        // Empty bytes ask the server for a full snapshot (its fresh-doc path).
        if vv.is_empty() {
            Vec::new()
        } else {
            vv.encode()
        }
    }

    async fn send(&self, message: &ProtocolMessage) -> Result<(), SyncError> {
        let bytes = encode(message).map_err(SyncError::Protocol)?;
        self.tx
            .send(bytes)
            .await
            .map_err(|_| SyncError::WebSocket("connection closed".into()))
    }

    async fn send_join_loro(&mut self, version: Vec<u8>) -> Result<(), SyncError> {
        // Arm the answer deadline BEFORE the frame leaves: an unanswered join
        // used to hang the session forever (2026-07-30). Joins default to
        // non-probe; the probe branch in run_session flags itself after.
        self.join_sent_at = Some(tokio::time::Instant::now());
        self.join_is_probe = false;
        // Auth rides the URL (`?token=`); the frame-level auth field is unused
        // by the edge.
        self.send(&ProtocolMessage::JoinRequest {
            crdt: CrdtType::Loro,
            room_id: self.room_id.clone(),
            auth: Vec::new(),
            version,
        })
        .await
    }

    /// Join (or re-join) the `%EPH` presence sub-room. Stamped so
    /// `run_session` can re-send on [`EPH_JOIN_RETRY`] until it is answered —
    /// the fire-and-forget version left presence dead for the session's whole
    /// life whenever this one frame was refused or swallowed (gh#126).
    async fn send_join_eph(&mut self) -> Result<(), SyncError> {
        self.eph_join_sent_at = Some(tokio::time::Instant::now());
        self.send(&ProtocolMessage::JoinRequest {
            crdt: CrdtType::LoroEphemeralStore,
            room_id: self.room_id.clone(),
            auth: Vec::new(),
            version: Vec::new(),
        })
        .await
    }

    async fn handle_frame(
        &mut self,
        bytes: &[u8],
        ready: &mut Option<oneshot::Sender<Result<(), SyncError>>>,
    ) -> Result<Option<SessionEnd>, SyncError> {
        let message = decode(bytes).map_err(SyncError::Protocol)?;
        // Advance the room-liveness clock for %LOR frames only (see
        // `last_lor_rx` for why %EPH must not count).
        let crdt = match &message {
            ProtocolMessage::JoinRequest { crdt, .. }
            | ProtocolMessage::JoinResponseOk { crdt, .. }
            | ProtocolMessage::JoinError { crdt, .. }
            | ProtocolMessage::DocUpdate { crdt, .. }
            | ProtocolMessage::DocUpdateFragmentHeader { crdt, .. }
            | ProtocolMessage::DocUpdateFragment { crdt, .. }
            | ProtocolMessage::Ack { crdt, .. }
            | ProtocolMessage::RoomError { crdt, .. }
            | ProtocolMessage::Leave { crdt, .. } => *crdt,
        };
        if crdt == CrdtType::Loro {
            self.last_lor_rx = tokio::time::Instant::now();
        }
        match message {
            ProtocolMessage::JoinResponseOk {
                crdt,
                version,
                permission,
                ..
            } => {
                self.on_join_ok(crdt, version, permission, ready).await?;
                Ok(None)
            }
            ProtocolMessage::JoinError {
                crdt,
                code,
                message,
                ..
            } => {
                if crdt == CrdtType::Loro {
                    if code == JoinErrorCode::VersionUnknown {
                        // Server can't diff from our VV — fall back to a full
                        // snapshot backfill.
                        self.request_full_resync().await?;
                        return Ok(None);
                    }
                    return Ok(Some(SessionEnd::Evicted(format!("{code:?}: {message}"))));
                }
                // Not fatal, and no longer fire-and-forget: `joined_eph` stays
                // false, so run_session re-sends the join on EPH_JOIN_RETRY.
                tracing::warn!(room = %self.room_id, ?code, %message, "ephemeral join failed; will retry");
                Ok(None)
            }
            ProtocolMessage::DocUpdate { crdt, updates, .. } => {
                self.apply_remote(crdt, updates).await?;
                Ok(None)
            }
            ProtocolMessage::DocUpdateFragmentHeader {
                crdt,
                batch_id,
                fragment_count,
                total_size_bytes,
                ..
            } => {
                if fragment_count == 0
                    || fragment_count > MAX_FRAGMENT_COUNT
                    || total_size_bytes as usize > MAX_REASSEMBLED_BYTES
                {
                    tracing::warn!(
                        room = %self.room_id,
                        fragment_count,
                        total_size_bytes,
                        "rejecting oversized fragment batch"
                    );
                    return Ok(None);
                }
                self.fragments.insert(
                    batch_id,
                    FragmentBuffer {
                        crdt,
                        parts: vec![None; fragment_count as usize],
                        received: 0,
                        total_size: total_size_bytes as usize,
                    },
                );
                Ok(None)
            }
            ProtocolMessage::DocUpdateFragment {
                batch_id,
                index,
                fragment,
                ..
            } => {
                self.on_fragment(batch_id, index, fragment).await?;
                Ok(None)
            }
            ProtocolMessage::Ack {
                crdt,
                ref_id,
                status,
                ..
            } => {
                self.on_ack(crdt, ref_id, status).await?;
                Ok(None)
            }
            ProtocolMessage::RoomError { code, message, .. } => match code {
                RoomErrorCode::Evicted => {
                    Ok(Some(SessionEnd::Evicted(format!("RoomError: {message}"))))
                }
                _ => {
                    // RejoinSuggested (or unknown): refresh both sub-rooms on
                    // this socket.
                    let version = self.local_version_bytes();
                    self.send_join_loro(version).await?;
                    Ok(None)
                }
            },
            // Server never sends these to us; ignore.
            ProtocolMessage::JoinRequest { .. } | ProtocolMessage::Leave { .. } => Ok(None),
        }
    }

    async fn on_join_ok(
        &mut self,
        crdt: CrdtType,
        version: Vec<u8>,
        _permission: Permission,
        ready: &mut Option<oneshot::Sender<Result<(), SyncError>>>,
    ) -> Result<(), SyncError> {
        match crdt {
            CrdtType::Loro => {
                self.join_sent_at = None; // join answered — disarm the deadline
                let was_probe = std::mem::take(&mut self.join_is_probe);
                let server_vv = if version.is_empty() {
                    VersionVector::default()
                } else {
                    VersionVector::decode(&version).map_err(|err| {
                        SyncError::Protocol(format!("invalid server version vector: {err}"))
                    })?
                };
                self.server_vv = Some(server_vv.clone());
                self.joined_lor = true;
                self.joined_at.get_or_insert_with(tokio::time::Instant::now);
                self.connected.set(true);
                // The version the room advertises IS proof of what it holds —
                // the strongest acknowledgement available, and the one a fresh
                // client would backfill from. Probe answers count too: that is
                // how a room converged by another device retires this device's
                // outbox rows without anything being sent.
                self.note_acknowledged(&server_vv);
                if was_probe {
                    // A probe or recovery answer on an established session
                    // proves only that the room is alive and advertises the
                    // snapshot VV. Do not publish the stale graph again or
                    // replay ordinary join side effects. If presence never
                    // joined, retain the existing one-shot repair.
                    if !self.joined_eph {
                        self.send_join_eph().await?;
                    }
                    return Ok(());
                }
                // Resubmit-from-VV: push everything the server lacks. This
                // covers both fresh docs (first upload) and updates that went
                // unacked across a reconnect or stale-peer resync. Gated on
                // the VERSION VECTORS, not on the export bytes:
                // `export(updates(&vv))` returns a non-empty envelope even
                // when there is nothing to say, so a byte-length gate made
                // every liveness probe upload a no-op DocUpdate that dirtied
                // the room's tail/backup caches and re-armed its daily alarm
                // — a fleet of idle rooms that could never actually go idle
                // (adversarial-review finding).
                if !self.doc.oplog_vv().is_empty() && self.invalid_rejoins < MAX_INVALID_REJOINS {
                    if !server_vv.includes_vv(&self.doc.oplog_vv()) {
                        let missing = self
                            .doc
                            .export(ExportMode::updates(&server_vv))
                            .map_err(|e| SyncError::Loro(e.to_string()))?;
                        if !missing.is_empty() {
                            self.send_loro_updates(vec![missing]).await?;
                        }
                    }
                }
                // Join presence once the doc room is up.
                self.send_join_eph().await?;
                if let Some(tx) = ready.take() {
                    let _ = tx.send(Ok(()));
                }
                let _ = self.events.send(RoomEvent::Connected);
            }
            CrdtType::LoroEphemeralStore => {
                self.joined_eph = true;
                self.presence.set(true);
                let all = self.eph.encode_all();
                if !all.is_empty() {
                    self.send_eph_updates(vec![all]).await?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn apply_remote(
        &mut self,
        crdt: CrdtType,
        updates: Vec<Vec<u8>>,
    ) -> Result<(), SyncError> {
        match crdt {
            CrdtType::Loro => {
                let mut imported = false;
                let mut incomplete = false;
                for update in updates {
                    if update.is_empty() {
                        continue;
                    }
                    let import = {
                        let gate = self.recovery.mutation_gate.clone();
                        let _guard = gate.as_ref().map(|gate| {
                            gate.lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                        });
                        self.doc.import(&update)
                    };
                    match import {
                        Ok(status) if status.pending.is_none() => imported = true,
                        Ok(status) => {
                            incomplete = true;
                            tracing::warn!(
                                room = %self.room_id,
                                pending = ?status.pending,
                                "remote update import is incomplete; attempting server snapshot reseed"
                            );
                            if self.try_reseed(&update).await? {
                                imported = true;
                                incomplete = false;
                            } else {
                                self.request_full_resync().await?;
                            }
                        }
                        Err(err) => {
                            incomplete = true;
                            tracing::warn!(room = %self.room_id, error = %err, "remote update import failed");
                            self.request_full_resync().await?;
                        }
                    }
                }
                if imported && !incomplete {
                    let _ = self.events.send(RoomEvent::RemoteUpdate);
                }
            }
            CrdtType::LoroEphemeralStore => {
                let mut applied = false;
                for update in updates {
                    if update.is_empty() {
                        continue;
                    }
                    match self.eph.apply(&update) {
                        Ok(()) => applied = true,
                        Err(err) => {
                            tracing::warn!(room = %self.room_id, error = %err, "ephemeral apply failed");
                        }
                    }
                }
                if applied {
                    let _ = self.events.send(RoomEvent::EphemeralUpdate);
                }
            }
            other => {
                tracing::warn!(room = %self.room_id, ?other, "update for unsupported crdt");
            }
        }
        Ok(())
    }

    async fn request_full_resync(&mut self) -> Result<(), SyncError> {
        if !self.full_resync_requested {
            self.full_resync_requested = true;
            let suppress_join_effects = self.joined_lor;
            self.send_join_loro(Vec::new()).await?;
            self.join_is_probe = suppress_join_effects;
        }
        Ok(())
    }

    /// Import `snapshot` into a fresh doc, validate it against the version the
    /// server advertised immediately before the backfill, hand it to
    /// [`crate::convergence`] — which quarantines the stale document and
    /// replays every locally committed semantic entry onto the replacement —
    /// and atomically move every room-owned reference/subscription across.
    ///
    /// The ORDER is the safety property (gh#483): quarantine is durable before
    /// anything is replaced, replay and its verification happen before the swap
    /// is visible to the owner, and the outbox rows survive until the edge's
    /// version proves it holds them. A failure anywhere before the swap leaves
    /// the stale document in place — unmergeable, but complete — which is the
    /// half of the fork that loses nothing.
    async fn try_reseed(&mut self, snapshot: &[u8]) -> Result<bool, SyncError> {
        let Some(on_reseed) = self.recovery.on_reseed.clone() else {
            // The public API requires an explicit owner. Test-only disabled
            // recovery may request one snapshot but must never swap behind a
            // caller that still retains the supplied LoroDoc.
            return Ok(false);
        };
        let Some(server_vv) = self.server_vv.clone() else {
            return Ok(false);
        };
        let candidate = LoroDoc::new();
        let status = match candidate.import(snapshot) {
            Ok(status) if status.pending.is_none() => status,
            Ok(_) | Err(_) => return Ok(false),
        };
        debug_assert!(status.pending.is_none());
        if candidate.oplog_vv() != server_vv {
            return Ok(false);
        }

        // This starts before the old subscription/doc are detached and ends
        // after the owner callback has installed the candidate. A local
        // publication therefore lands wholly before the cut or wholly after
        // it; it cannot commit into a detached old document.
        let gate = self.recovery.mutation_gate.clone();
        let guard = gate.as_ref().map(|gate| {
            gate.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });

        let report = match self.convergence.recover(&self.doc, &candidate, &server_vv) {
            Ok(report) => report,
            Err(err) => {
                // The stale document is untouched and the quarantine (if the
                // failure came after it was written) still holds it. Say so
                // loudly and stay unmerged rather than swap into a document
                // that cannot account for local content.
                tracing::error!(
                    room = %self.room_id,
                    error = %err,
                    "convergence recovery refused the server snapshot; keeping the local document"
                );
                self.blocked = Some(format!("recovery refused the server snapshot: {err}"));
                drop(guard);
                self.publish_convergence();
                return Ok(false);
            }
        };
        let replayed = report.total();

        // Replay happened before subscribing, so explicitly derive the only
        // local delta the server can lack. This also avoids publishing the
        // server snapshot back to the server.
        let replay_update = if replayed > 0 {
            Some(
                candidate
                    .export(ExportMode::updates(&server_vv))
                    .map_err(|err| SyncError::Loro(err.to_string()))?,
            )
        } else {
            None
        };

        // Anything derived from the stale graph is now invalid: discard sent
        // batches and make queued subscription callbacks self-identify as the
        // old generation. Only the replayed semantic content crosses the cut —
        // as content, re-committed on the replacement, never as the operations
        // the edge has already refused.
        self.pending.clear();
        self.invalid_rejoins = 0;
        // A completed reseed is the repair for a refused history.
        self.blocked = None;
        let generation = self.doc_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let local_tx = self.local_tx.clone();
        let subscription = candidate.subscribe_local_update(Box::new(move |bytes: &Vec<u8>| {
            let _ = local_tx.send((generation, bytes.clone()));
            true
        }));
        *self
            .doc_sub
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(subscription);
        *self
            .current_doc
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = candidate.clone();
        on_reseed(candidate.clone());
        self.doc = candidate;
        drop(guard);

        if let Some(update) = replay_update.filter(|bytes| !bytes.is_empty()) {
            self.send_loro_updates(vec![update]).await?;
        }
        tracing::warn!(
            room = %self.room_id,
            device = self.recovery.local_device_id.as_deref().unwrap_or("-"),
            restored_messages = report.restored_messages.len(),
            extended_messages = report.extended_messages.len(),
            restored_commands = report.restored_commands.len(),
            resolved_commands = report.resolved_commands.len(),
            diverged = report.diverged.len(),
            "reseeded stale local document from validated server snapshot and replayed \
             local semantic content"
        );
        if !report.diverged.is_empty() {
            // Not data loss — the authoritative copy carries more than ours and
            // the quarantine holds ours — but it is the one case an operator
            // may want to look at, so it is named rather than counted away.
            tracing::warn!(
                room = %self.room_id,
                ids = ?report.diverged,
                "authoritative copies of these entries are ahead of the local ones; \
                 local bytes retained in the quarantine"
            );
        }
        self.publish_convergence();
        Ok(true)
    }

    async fn on_fragment(
        &mut self,
        batch_id: BatchId,
        index: u64,
        fragment: Vec<u8>,
    ) -> Result<(), SyncError> {
        let Some(buffer) = self.fragments.get_mut(&batch_id) else {
            // Header never seen (or batch rejected) — nothing to assemble;
            // unlike the DO we hold no durable state, so just drop it.
            return Ok(());
        };
        let index = index as usize;
        if index >= buffer.parts.len() {
            self.fragments.remove(&batch_id);
            return Ok(());
        }
        if buffer.parts[index].is_none() {
            buffer.received += 1;
        }
        buffer.parts[index] = Some(fragment);
        if buffer.received < buffer.parts.len() {
            return Ok(());
        }
        let Some(buffer) = self.fragments.remove(&batch_id) else {
            return Ok(());
        };
        let mut total = Vec::with_capacity(buffer.total_size);
        for part in buffer.parts.into_iter().flatten() {
            total.extend_from_slice(&part);
        }
        self.apply_remote(buffer.crdt, vec![total]).await
    }

    async fn on_ack(
        &mut self,
        crdt: CrdtType,
        ref_id: BatchId,
        status: UpdateStatusCode,
    ) -> Result<(), SyncError> {
        match status {
            UpdateStatusCode::Ok => {
                if let Some(batch) = self.pending.remove(&ref_id)
                    && crdt == CrdtType::Loro
                    && let Some(version) = Self::batch_version(&batch)
                {
                    self.note_acknowledged(&version);
                }
            }
            UpdateStatusCode::FragmentTimeout => {
                // DO hibernated mid-batch and lost reassembly state — resend
                // the whole batch (self-healing per the edge's design).
                if let Some(batch) = self.pending.remove(&ref_id) {
                    self.send_loro_updates(batch).await?;
                }
            }
            UpdateStatusCode::InvalidUpdate | UpdateStatusCode::PermissionDenied => {
                // A successful reseed clears every batch derived from the old
                // graph. Its delayed rejection is historical, not a reason to
                // rejoin or resubmit the replacement.
                if self.pending.remove(&ref_id).is_none() {
                    return Ok(());
                }
                if crdt == CrdtType::Loro {
                    if self.invalid_rejoins >= MAX_INVALID_REJOINS {
                        tracing::error!(
                            room = %self.room_id,
                            "updates repeatedly rejected (stale peer past shallow start); giving up resubmission"
                        );
                        // The room is live and will not take this device's
                        // history. Before gh#483 that state was invisible —
                        // every surface said "connected" while the document
                        // stopped converging for good. Name it.
                        self.blocked = Some(
                            "the edge refuses this device's history (shallow start is newer) \
                             and no server snapshot has repaired it"
                                .into(),
                        );
                        self.publish_convergence();
                        return Ok(());
                    }
                    self.invalid_rejoins += 1;
                    // §3.1 stale peer: resync fresh (rejoin with our VV pulls
                    // the server's post-trim state), then the JoinResponseOk
                    // handler resubmits from the server's VV.
                    let version = self.local_version_bytes();
                    self.send_join_loro(version).await?;
                } else {
                    tracing::warn!(room = %self.room_id, ?crdt, ?status, "update rejected");
                }
            }
            UpdateStatusCode::PayloadTooLarge => {
                self.pending.remove(&ref_id);
                tracing::error!(room = %self.room_id, "server rejected update as too large");
            }
            other => {
                self.pending.remove(&ref_id);
                tracing::warn!(room = %self.room_id, ?other, "unexpected ack status");
            }
        }
        Ok(())
    }

    /// Send loro updates, batching small ones and fragmenting any single
    /// update above the protocol payload budget. Every batch is tracked in
    /// `pending` until its Ack.
    async fn send_loro_updates(&mut self, updates: Vec<Vec<u8>>) -> Result<(), SyncError> {
        let mut small: Vec<Vec<u8>> = Vec::new();
        let mut small_bytes = 0usize;
        for update in updates {
            if update.is_empty() {
                continue;
            }
            if update.len() > FRAGMENT_BYTES {
                self.send_fragmented(update).await?;
                continue;
            }
            if small_bytes + update.len() > FRAGMENT_BYTES {
                self.flush_small_batch(std::mem::take(&mut small)).await?;
                small_bytes = 0;
            }
            small_bytes += update.len();
            small.push(update);
        }
        if !small.is_empty() {
            self.flush_small_batch(small).await?;
        }
        Ok(())
    }

    async fn flush_small_batch(&mut self, updates: Vec<Vec<u8>>) -> Result<(), SyncError> {
        let batch_id = new_batch_id();
        self.pending.insert(batch_id, updates.clone());
        self.send(&ProtocolMessage::DocUpdate {
            crdt: CrdtType::Loro,
            room_id: self.room_id.clone(),
            updates,
            batch_id,
        })
        .await
    }

    async fn send_fragmented(&mut self, update: Vec<u8>) -> Result<(), SyncError> {
        let batch_id = new_batch_id();
        self.pending.insert(batch_id, vec![update.clone()]);
        let fragment_count = update.len().div_ceil(FRAGMENT_BYTES);
        self.send(&ProtocolMessage::DocUpdateFragmentHeader {
            crdt: CrdtType::Loro,
            room_id: self.room_id.clone(),
            batch_id,
            fragment_count: fragment_count as u64,
            total_size_bytes: update.len() as u64,
        })
        .await?;
        for (index, chunk) in update.chunks(FRAGMENT_BYTES).enumerate() {
            self.send(&ProtocolMessage::DocUpdateFragment {
                crdt: CrdtType::Loro,
                room_id: self.room_id.clone(),
                batch_id,
                index: index as u64,
                fragment: chunk.to_vec(),
            })
            .await?;
        }
        Ok(())
    }

    async fn send_eph_updates(&mut self, updates: Vec<Vec<u8>>) -> Result<(), SyncError> {
        let updates: Vec<Vec<u8>> = updates.into_iter().filter(|u| !u.is_empty()).collect();
        if updates.is_empty() {
            return Ok(());
        }
        // Presence payloads are tiny; no fragmentation or resend tracking.
        self.send(&ProtocolMessage::DocUpdate {
            crdt: CrdtType::LoroEphemeralStore,
            room_id: self.room_id.clone(),
            updates,
            batch_id: new_batch_id(),
        })
        .await
    }
}

fn new_batch_id() -> BatchId {
    let uuid = uuid::Uuid::new_v4();
    let bytes = uuid.as_bytes();
    let mut id = [0u8; 8];
    id.copy_from_slice(&bytes[..8]);
    BatchId(id)
}

#[cfg(test)]
mod tests;
