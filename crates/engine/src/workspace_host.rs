//! WorkspaceHost — owns the per-user `WorkspaceDoc` (ARCHITECTURE §2.2, made
//! per-user for privacy): local snapshot persistence, edge room sync
//! (`ws4/{orgId}/{userId}`, offline-tolerant — spaces/sessions are private to
//! their owner, never org-visible), the device registry row for THIS device,
//! and the typed watch channels the WatchChats/WatchDevices/WatchSessions RPC
//! streams are fed from.
//!
//! It also owns the ORG-wide device registry ([`crate::org_devices`], gh#66):
//! devices are the one index a team must share, since a teammate who cannot see
//! the box cannot reach it. `WatchDevices` publishes the union, presence beats
//! on both rooms, and every device write lands in both docs.
//!
//! Writer discipline (kept from the doc schema): this host writes its own device row,
//! its own session-status rows, and rows for chats it hosts; renames/archives are LWW
//! sets accepted from any device (the Mutate surface).
//!
//! Liveness: `lastSeenAt` is a map write on boot/shutdown ONLY — staying online
//! never grows the workspace oplog. Between those two writes the value published
//! on `WatchDevices` is an OVERLAY: `now` for every device this engine believes
//! connected, and the doc's own stamp for every device it does not. What that
//! belief is made of, and why it is no longer a 15s heartbeat, is
//! [`crate::presence`] (gh#145).

use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, Weak};

use chrono::Utc;
use tokio::sync::watch;

use comet_doc::{DeletedSpace, WorkspaceDoc};
use comet_proto::{Chat, ChatConfig, Device, Session, Space};
use comet_sync::{DocRecovery, DocsStore, RoomClient};

use crate::doc_host::EdgeConfig;
use crate::org_devices::{OrgDevices, OrgDevicesConfig, merge_devices};
use crate::presence::{PresenceBelief, PresenceRoom, room_presence_ids};
use crate::{EngineError, now_ms};

/// Snapshot row id in the local `DocsStore` (chat ids never collide with it).
/// `workspace2` = the spaces-overhaul destructive break: the legacy `workspace`
/// row is simply never read again. (The per-user room break — `ws2/{orgId}` →
/// `ws3/{orgId}/{userId}` — needed no row-id bump: the local store itself moved
/// to `orgs/{org}/{user}/`, so the old snapshot is unreachable anyway.)
///
/// A ROOM generation bump must never bump this row (gh#148, `ws3/` → `ws4/`).
/// The edge is not authoritative for the workspace doc — this local snapshot
/// is. Abandoning a room's storage is survivable precisely because every
/// device still holds the doc here and re-uploads it into the virgin room on
/// first join ([`RoomActor`]'s resubmit-from-version-vector path). Bump both
/// at once and there is nothing left to re-seed FROM: an edge-side break that
/// loses nothing becomes real data loss on every device simultaneously.
pub const WORKSPACE_DOC_ID: &str = "workspace2";
/// Legacy (pre-spaces) snapshot row — best-effort deleted on open.
const LEGACY_WORKSPACE_DOC_ID: &str = "workspace";
/// Org used when none is configured (matches the edge's dev-mode `user@org` bearers).
pub const DEFAULT_ORG_ID: &str = "dev-org";
/// User used when none is configured (dev mode without a bearer).
pub const DEFAULT_USER_ID: &str = "dev-user";
/// Republish cadence for the device watch.
///
/// Was the ephemeral presence beat (gh#145 deleted that); it is now purely
/// local — no frame leaves this process. It exists because the overlay stamps
/// `lastSeenAt = now` for believed-connected devices, and every reader's online
/// window ([`comet_proto::view::PRESENCE_STALE_MS`], 70s) is measured against
/// that stamp: republishing keeps a still-connected device's badge from ageing
/// out between events, and lets one that stopped being believed age out at all.
const PUBLISH_INTERVAL_MS: u64 = 15_000;
/// Relay-status probe cadence — the ONE periodic network beat presence still
/// has, and it deliberately runs against `DeviceRoom` rather than the sync
/// rooms.
///
/// That choice is the whole of gh#145. `DeviceRoom` derives host liveness from
/// `getWebSocketAutoResponseTimestamp`, so answering `GET /device/{id}/status`
/// costs ~2ms of Durable Object duration and no wake worth the name — measured,
/// on the same day the sync rooms burned 83% of the daily free tier answering
/// heartbeats. `SessionRoom` is now asked nothing on a timer at all.
///
/// The probe used to run only for devices whose heartbeat had gone stale. With
/// no heartbeat left it runs for every remote device, every interval: it is
/// what turns a stale-positive room answer (a socket whose uplink died silently
/// and which the runtime therefore never closes) back into "offline", and
/// nothing else can.
const RELAY_PROBE_INTERVAL_MS: u64 = 30_000;
/// Per-request timeout for a relay-status probe.
const RELAY_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Debounce window for local snapshot saves after a doc change.
const SNAPSHOT_DEBOUNCE_MS: u64 = 1_000;
/// Initial-join retry backoff (base, cap). A first workspace-room join that
/// fails must not strand the device offline until an app restart — retry until
/// it lands. Jittered so N devices restarting together don't resynchronize
/// their retries into a thundering herd on the cold DO.
const JOIN_RETRY_BASE: std::time::Duration = std::time::Duration::from_millis(500);
const JOIN_RETRY_CAP: std::time::Duration = std::time::Duration::from_secs(30);
/// How long a supervised `RoomClient` may hold ZERO live connections before it
/// is thrown away and rebuilt (gh#116).
///
/// The client's own redial loop backs off to 30s, so an ordinary edge deploy is
/// back inside a minute and never reaches this. Three minutes dark means the
/// actor is not making progress — a wedge this layer cannot see into (a write
/// blocked on a half-open socket, a poisoned dial, a task that died) — and the
/// only cure that worked in production was a process restart. This is that
/// restart, scoped to one room: drop the client, dial a fresh one with fresh
/// credentials. Cheap enough that a genuinely offline box paying it every three
/// minutes costs one extra connect attempt per room.
const ROOM_DARK_REBUILD: std::time::Duration = std::time::Duration::from_secs(180);

/// Cheap decorrelation jitter (0–500ms) without pulling in a rng — derived from
/// the sub-nanosecond wall clock. Mirrors the device relay's `jitter()`.
fn join_retry_jitter() -> std::time::Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    std::time::Duration::from_millis(u64::from(nanos) % 500)
}

#[derive(Debug, Clone)]
pub struct WorkspaceHostConfig {
    pub device_id: String,
    /// Human name for this device's registry row (hostname by default).
    pub device_name: String,
    /// `std::env::consts::OS`-style platform string.
    pub platform: String,
    pub org_id: String,
    /// The signed-in user — workspace docs are per-user (`ws4/{orgId}/{userId}`):
    /// spaces/sessions are private to their owner, never org-visible.
    pub user_id: String,
    /// When present, the host joins `/workspace/{orgId}/ws`. `None` = fully offline
    /// (local snapshots only; the doc still drives everything device-side).
    pub edge: Option<EdgeConfig>,
}

struct WorkspaceHostInner {
    store: Arc<DocsStore>,
    config: WorkspaceHostConfig,
    doc: RwLock<Arc<WorkspaceDoc>>,
    changed_tx: watch::Sender<u64>,
    /// The org-wide device registry (gh#66) — the only entity index that is
    /// NOT private to this user. Shares this host's change channel, snapshot
    /// debounce and presence tick.
    org_devices: OrgDevices,
    chats_tx: watch::Sender<Vec<Chat>>,
    devices_tx: watch::Sender<Vec<Device>>,
    sessions_tx: watch::Sender<Vec<Session>>,
    spaces_tx: watch::Sender<Vec<Space>>,
    room: Arc<Mutex<Option<RoomClient>>>,
    /// Which devices this engine believes are connected, and on what evidence
    /// ([`crate::presence`]). Sticky by design: a room that says nothing —
    /// which, with the heartbeat gone, is what an idle night looks like — must
    /// never be read as every device having left (gh#126).
    presence: Mutex<PresenceBelief>,
    /// Called with a device id whenever its presence heartbeat proves it alive —
    /// wired to `LinkCache::reset_cooldown` so a peer that comes back is dialed
    /// immediately instead of waiting out the failure backoff.
    peer_alive: Mutex<Option<PeerAliveHook>>,
    /// Doc subscription (drop = unsubscribe) — bumps the change watch on every commit.
    _sub: Mutex<loro::Subscription>,
}

impl WorkspaceHostInner {
    fn doc(&self) -> Arc<WorkspaceDoc> {
        self.doc
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn reseed(&self, raw: loro::LoroDoc) {
        let replacement = Arc::new(WorkspaceDoc::from_doc(raw));
        let changed_tx = self.changed_tx.clone();
        let sub = replacement.doc().subscribe_root(Arc::new(move |_diff| {
            changed_tx.send_modify(|value| *value = value.wrapping_add(1));
        }));
        *self.doc.write().unwrap_or_else(PoisonError::into_inner) = replacement;
        *lock(&self._sub) = sub;
        self.changed_tx
            .send_modify(|value| *value = value.wrapping_add(1));
    }
}

/// "This peer is alive" callback (device id) — see `WorkspaceHost::set_peer_alive_hook`.
pub type PeerAliveHook = Arc<dyn Fn(&str) + Send + Sync>;

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Why a supervised room client was abandoned. Transient exits rebuild it;
/// deterministic malformed protocol parks it until explicit restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoomExit {
    /// The client's actor task is gone (clean shutdown, or a panic / lost
    /// local-update channel that leaves a client which can never reconnect).
    ActorGone,
    /// The client is alive and has held no live connection for
    /// [`ROOM_DARK_REBUILD`]. Its own redial loop caps at 30s, so this is not
    /// slowness — it is a stuck actor (gh#116), and a fresh client with fresh
    /// credentials is the way out.
    DarkTooLong,
    /// The server produced deterministic malformed protocol bytes. Rebuilding
    /// would wake the same DO forever, so this room parks until its owner is
    /// explicitly restarted.
    ProtocolParked,
}

/// Join `room_id` and hold it in `slot` for the joiner's life — the shared
/// machinery behind the per-user workspace room, the org device registry room,
/// and every per-chat session room.
///
/// This is a SUPERVISOR, not a one-shot join, because every layer under it can
/// fail in a way that looks like success from here:
///
/// - `RoomClient` only self-reconnects AFTER a first successful join; an
///   INITIAL failure (a 500 from an overloaded DO, a token racing a refresh, an
///   edge deploy) used to end the task and leave the device offline until an
///   app restart — presence stuck "offline" while the relay and per-chat rooms
///   worked fine. The first join therefore retries on a capped, jittered
///   backoff.
/// - An ESTABLISHED client whose actor dies, or wedges, used to be invisible:
///   the slot still held `Some(client)`, so every "connected?" answer stayed
///   true while zero sockets were alive. That is the gh#116 incident — an edge
///   redeploy cycled the DOs at 10:06 and the box was dark until somebody
///   restarted the daemon 25 minutes later. Both shapes now end the client and
///   rebuild it here.
///
/// `slot` is held weakly: dropping the host drops its room Arc, which is what
/// ends this task. `on_event` runs on every ephemeral update — which, since
/// gh#145, is the room telling us who it can see.
///
/// There is deliberately no join side effect any more. It used to be the
/// presence beat: publish `presence/{ourDeviceId}` on every (re)join, because
/// the ephemeral store's 30s TTL had forgotten our entry. Nothing is published
/// on join now — the edge derives presence from the socket the join arrived on.
pub(crate) fn spawn_room_join(
    url: Arc<dyn comet_sync::UrlProvider>,
    room_id: String,
    doc: loro::LoroDoc,
    local_device_id: Option<String>,
    on_reseed: Arc<dyn Fn(loro::LoroDoc) + Send + Sync>,
    slot: Weak<Mutex<Option<RoomClient>>>,
    on_event: Arc<dyn Fn() + Send + Sync>,
) {
    tokio::spawn(async move {
        let mut backoff = JOIN_RETRY_BASE;
        // Outlives any one RoomClient actor. If supervision rebuilds a client
        // after a successful reseed, it must dial with the replacement rather
        // than resurrecting the stale handle captured at startup.
        let current_doc = Arc::new(RwLock::new(doc));
        loop {
            if slot.upgrade().is_none() {
                return; // host dropped
            }
            let doc = current_doc
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            let reseed_slot = current_doc.clone();
            let reseed_owner = on_reseed.clone();
            let reseed = Arc::new(move |replacement: loro::LoroDoc| {
                *reseed_slot.write().unwrap_or_else(PoisonError::into_inner) = replacement.clone();
                reseed_owner(replacement);
            });
            let recovery = match local_device_id.as_deref() {
                Some(device_id) => DocRecovery::for_device(device_id, reseed),
                None => DocRecovery::replacing(reseed),
            };
            match RoomClient::connect_via(url.clone(), &room_id, doc, recovery).await {
                Ok(client) => {
                    // Health handles are taken BEFORE the client moves into the
                    // slot, so supervision never has to hold the slot lock.
                    let events = client.events();
                    let health = client.watch_connected();
                    let connected = client.connected();
                    let Some(room) = slot.upgrade() else { return };
                    *lock(&room) = Some(client);
                    tracing::info!(room = %room_id, "room joined");
                    drop(room);
                    // A join landed: the next failure starts from the floor.
                    backoff = JOIN_RETRY_BASE;
                    let exit = supervise_room(&room_id, events, health, connected, &on_event).await;
                    let Some(room) = slot.upgrade() else { return };
                    // Dropping the client aborts its actor and closes its
                    // sockets; the next loop turn dials a brand-new one.
                    let previous = lock(&room).take();
                    drop(room);
                    drop(previous);
                    match exit {
                        RoomExit::ActorGone => {
                            tracing::warn!(room = %room_id, "room client ended; rebuilding")
                        }
                        RoomExit::DarkTooLong => tracing::warn!(
                            room = %room_id,
                            dark_s = ROOM_DARK_REBUILD.as_secs(),
                            "room held no live connection; rebuilding the client"
                        ),
                        RoomExit::ProtocolParked => {
                            tracing::error!(room = %room_id,
                                "room parked after malformed protocol; explicit restart required");
                            return;
                        }
                    }
                }
                Err(err) if err.is_permanent() => {
                    tracing::error!(room = %room_id, error = %err,
                        "initial room join returned malformed protocol; parking without retry");
                    return;
                }
                Err(err) => {
                    // Taper the level once the backoff has reached its cap.
                    // Every open chat is supervised now, so a box that loses
                    // its uplink for a night would otherwise write one WARN per
                    // chat per 30s into journald forever — the first handful of
                    // attempts is the part anyone reads.
                    if backoff < JOIN_RETRY_CAP {
                        tracing::warn!(room = %room_id, error = %err, backoff_ms = backoff.as_millis() as u64,
                            "room join failed; retrying");
                    } else {
                        tracing::debug!(room = %room_id, error = %err, "room join still failing; retrying");
                    }
                }
            }
            tokio::time::sleep(backoff + join_retry_jitter()).await;
            backoff = (backoff * 2).min(JOIN_RETRY_CAP);
        }
    });
}

/// Watch one established client until it needs replacing.
///
/// Returns on actor death or on [`ROOM_DARK_REBUILD`] without a live
/// connection. A client that merely drops and redials — the ordinary edge
/// deploy — is left alone: its own backoff loop is faster than anything this
/// could do, and the reconnect is logged rather than acted on.
async fn supervise_room(
    room_id: &str,
    mut events: tokio::sync::broadcast::Receiver<comet_sync::RoomEvent>,
    mut health: tokio::sync::watch::Receiver<bool>,
    initially_connected: bool,
    on_event: &Arc<dyn Fn() + Send + Sync>,
) -> RoomExit {
    let mut dark_since = (!initially_connected).then(tokio::time::Instant::now);
    loop {
        let dark_deadline = dark_since.map(|at| at + ROOM_DARK_REBUILD);
        tokio::select! {
            event = events.recv() => match event {
                Ok(comet_sync::RoomEvent::EphemeralUpdate) => on_event(),
                Ok(comet_sync::RoomEvent::ProtocolParked) => return RoomExit::ProtocolParked,
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return RoomExit::ActorGone,
            },
            changed = health.changed() => {
                if changed.is_err() {
                    return RoomExit::ActorGone; // the actor dropped its flag
                }
                let connected = *health.borrow_and_update();
                if connected {
                    if dark_since.is_some() {
                        tracing::info!(room = %room_id, "room reconnected");
                    }
                    dark_since = None;
                    // A reconnect is a fresh socket on the room, so the edge
                    // recomputes and re-announces presence by itself (gh#145).
                    // Republish anyway: our OWN belief may be stale in the other
                    // direction, and the answer is already on its way.
                    on_event();
                } else {
                    tracing::warn!(room = %room_id, "room connection down; client is redialing");
                    dark_since = Some(tokio::time::Instant::now());
                }
            }
            _ = async {
                match dark_deadline {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending::<()>().await,
                }
            } => return RoomExit::DarkTooLong,
        }
    }
}

#[derive(Clone)]
pub struct WorkspaceHost {
    inner: Arc<WorkspaceHostInner>,
}

impl WorkspaceHost {
    /// Load (or init) the workspace doc, upsert this device's registry row, start the
    /// change-driven task, and join the edge workspace room when configured.
    pub fn open(store: Arc<DocsStore>, config: WorkspaceHostConfig) -> Result<Self, EngineError> {
        let doc = match store.load_snapshot(WORKSPACE_DOC_ID)? {
            Some(bytes) => {
                let raw = loro::LoroDoc::new();
                raw.import(&bytes).map_err(|e| {
                    EngineError::Other(format!("workspace snapshot import failed: {e}"))
                })?;
                WorkspaceDoc::from_doc(raw)
            }
            None => WorkspaceDoc::new(),
        };
        let doc = Arc::new(doc);
        // Destructive-break hygiene: drop the unreachable legacy snapshot row and
        // stamp the in-band schema version for the NEXT break to detect.
        store.delete_snapshot(LEGACY_WORKSPACE_DOC_ID).ok();
        doc.ensure_schema_version()?;

        let (changed_tx, changed_rx) = watch::channel(0u64);
        let sub = doc.doc().subscribe_root(Arc::new({
            let changed_tx = changed_tx.clone();
            move |_diff| {
                changed_tx.send_modify(|v| *v = v.wrapping_add(1));
            }
        }));
        // The org registry rides the same change channel, so a teammate's
        // device row republishes `WatchDevices` exactly like a local write.
        let org_devices = OrgDevices::open(
            store.clone(),
            OrgDevicesConfig {
                device_id: config.device_id.clone(),
                org_id: config.org_id.clone(),
                edge: config.edge.clone(),
            },
            changed_tx.clone(),
        )?;

        // Boot: upsert our own device row — into BOTH the private workspace doc
        // (this user's own fleet) and the org registry (everyone's). A user-set
        // name (RenameDevice is LWW from any device) survives restarts; only a
        // missing row gets the hostname.
        let now = Utc::now();
        let existing = org_devices.device(&config.device_id).or_else(|| {
            doc.read_devices()
                .ok()?
                .into_iter()
                .find(|d| d.id == config.device_id)
        });
        let row = Device {
            id: config.device_id.clone(),
            name: existing
                .as_ref()
                .map(|d| d.name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| config.device_name.clone()),
            platform: config.platform.clone(),
            last_seen_at: Some(now),
            // First registration stamps `createdAt`; restarts keep the original
            // (the Devices page "Added …" fragment).
            created_at: existing.and_then(|d| d.created_at).or(Some(now)),
            // Every boot restamps the running binary's version (fleet staleness
            // on the Devices page; workspace version — same for every crate).
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
        };
        doc.upsert_device(&row)?;
        org_devices.upsert_device(&row);

        let state = doc.read_all()?;
        let (chats_tx, _) = watch::channel(state.chats);
        let (devices_tx, _) =
            watch::channel(merge_devices(state.devices, org_devices.read_devices()));
        let (sessions_tx, _) = watch::channel(state.sessions);
        let (spaces_tx, _) = watch::channel(state.spaces);

        let host = Self {
            inner: Arc::new(WorkspaceHostInner {
                store,
                config,
                doc: RwLock::new(doc),
                changed_tx,
                org_devices,
                chats_tx,
                devices_tx,
                sessions_tx,
                spaces_tx,
                room: Arc::new(Mutex::new(None)),
                presence: Mutex::new(PresenceBelief::default()),
                peer_alive: Mutex::new(None),
                _sub: Mutex::new(sub),
            }),
        };
        host.join_room();
        // The org registry's answer means the same thing as our own room's, for
        // a different fleet: fold it in and republish, so a teammate's box
        // coming online lights up at once instead of on the next tick.
        let weak = Arc::downgrade(&host.inner);
        host.inner.org_devices.set_presence_hook(Arc::new(move || {
            if let Some(inner) = weak.upgrade() {
                if let Some(ids) = inner.org_devices.presence_ids() {
                    lock(&inner.presence).room_answered(PresenceRoom::Org, ids);
                }
                inner.publish();
            }
        }));
        tokio::spawn(workspace_task(Arc::downgrade(&host.inner), changed_rx));
        if host.inner.config.edge.is_some() {
            tokio::spawn(relay_probe_task(Arc::downgrade(&host.inner)));
        }
        Ok(host)
    }

    /// Edge room join — offline-tolerant: a failed join logs and stays local-first.
    fn join_room(&self) {
        let Some(edge) = &self.inner.config.edge else {
            return;
        };
        let org_id = self.inner.config.org_id.clone();
        // Per-dial URL provider: the bearer is re-read on every (re)connect.
        // The device id rides along so the edge can derive presence from this
        // socket rather than be told over `%EPH` every 15s (gh#145).
        let url = edge.room_url_as(
            format!("/workspace/{org_id}/ws"),
            Some(&self.inner.config.device_id),
        );
        // The room id carried inside our protocol frames. It is a LABEL, not
        // an address: the URL above is what selects the Durable Object, and
        // the edge derives the real room name (`ws{n}/{orgId}/{userId}`) from
        // the caller's own auth claim, so a mismatched user can never join
        // regardless of what we write here.
        //
        // Keeping the generation in step with the edge is therefore hygiene
        // for logs and health output, NOT a correctness requirement — and that
        // is deliberate (gh#148). A generation bump has to be a transparent
        // migration: an engine that has not been updated still lands in the
        // new room and converges, because nothing on either side routes on
        // this string. Our engines retry a failed join forever, so a version
        // skew that DID matter would be a silent outage, not an error.
        let room_id = format!("ws4/{}/{}", org_id, self.inner.config.user_id);
        let weak = Arc::downgrade(&self.inner);
        let reseed_owner = weak.clone();
        spawn_room_join(
            url,
            room_id,
            self.inner.doc().doc().clone(),
            None,
            Arc::new(move |replacement| {
                if let Some(inner) = reseed_owner.upgrade() {
                    inner.reseed(replacement);
                }
            }),
            Arc::downgrade(&self.inner.room),
            // Presence rides `%EPH`, never the doc — the room's answer must
            // re-publish the device watch itself (the signal that distinguishes
            // "host offline" from slow sync).
            Arc::new(move || {
                if let Some(inner) = weak.upgrade() {
                    inner.absorb_workspace_presence();
                    inner.publish();
                }
            }),
        );
    }

    /// Wire the "peer is alive" signal (fresh presence heartbeat) to a callback —
    /// the engine points this at `LinkCache::reset_cooldown`.
    pub fn set_peer_alive_hook(&self, hook: PeerAliveHook) {
        *lock(&self.inner.peer_alive) = Some(hook);
    }

    pub fn device_id(&self) -> &str {
        &self.inner.config.device_id
    }

    pub fn doc(&self) -> Arc<WorkspaceDoc> {
        self.inner.doc()
    }

    pub fn doc_arc(&self) -> Arc<WorkspaceDoc> {
        self.inner.doc()
    }

    /// Is the workspace room joined RIGHT NOW? Holding a client is not holding
    /// a room — see [`comet_sync::RoomClient::connected`].
    pub fn connected(&self) -> bool {
        lock(&self.inner.room)
            .as_ref()
            .is_some_and(|client| client.connected())
    }

    /// Is the workspace room's `%EPH` presence sub-room joined RIGHT NOW?
    /// Narrower than [`Self::connected`]: a doc-live room with dead presence
    /// silently drops every heartbeat this device sends (gh#126), and the
    /// health census has to be able to say so.
    pub fn presence_connected(&self) -> bool {
        lock(&self.inner.room)
            .as_ref()
            .is_some_and(|client| client.presence_joined())
    }

    // ── watches (WatchChats / WatchDevices / merged WatchSessions) ──────────

    pub fn watch_chats(&self) -> watch::Receiver<Vec<Chat>> {
        self.inner.chats_tx.subscribe()
    }

    pub fn watch_devices(&self) -> watch::Receiver<Vec<Device>> {
        self.inner.devices_tx.subscribe()
    }

    /// Raw workspace session-status rows (all devices').
    pub fn watch_session_rows(&self) -> watch::Receiver<Vec<Session>> {
        self.inner.sessions_tx.subscribe()
    }

    pub fn watch_spaces(&self) -> watch::Receiver<Vec<Space>> {
        self.inner.spaces_tx.subscribe()
    }

    /// WatchSessions source: remote devices' rows from the workspace doc merged with
    /// this engine's live status watch (the local view is fresher for our own runs).
    pub fn merged_sessions_watch(
        &self,
        local: watch::Receiver<Vec<Session>>,
    ) -> watch::Receiver<Vec<Session>> {
        let mut rows = self.watch_session_rows();
        let mut local = local;
        let device_id = self.inner.config.device_id.clone();
        let (tx, rx) = watch::channel(merge_sessions(&device_id, &rows.borrow(), &local.borrow()));
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    changed = rows.changed() => if changed.is_err() { break },
                    changed = local.changed() => if changed.is_err() { break },
                }
                let merged = merge_sessions(
                    &device_id,
                    &rows.borrow_and_update(),
                    &local.borrow_and_update(),
                );
                if tx.send(merged).is_err() {
                    break; // no receivers left
                }
            }
        });
        rx
    }

    // ── chat ownership (replaces the M2 "host everything" pragmatism) ───────

    /// §2.2 writer discipline: the chat's host is its row's `deviceId`. Unknown chats
    /// are claimable — the first run command claims them via [`Self::claim_chat`].
    pub fn is_host(&self, chat_id: &str) -> bool {
        match self.inner.doc().chat(chat_id) {
            Ok(Some(chat)) => chat.device_id == self.inner.config.device_id,
            Ok(None) => true,
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "workspace chat read failed");
                true
            }
        }
    }

    /// Claim-on-first-command: create the chat row under OUR device id when a run
    /// command arrives for a chat with no row yet. No-op when the row exists.
    ///
    /// Spaces invariant: every chat belongs to a space, so the claim resolves an
    /// own-device space matching `cwd` — or auto-creates one (gitDetected false;
    /// SpacesSync corrects on its next pass). A cwd-less claim (e.g. note_message
    /// racing ahead of the run command) leaves `spaceId` unset; the row is
    /// invisible to the UI until a spaced claim/create lands. NOTE: a worktree
    /// cwd claims a space *at the worktree path*, not the repo root — acceptable
    /// for tooling-only (raw doc command) traffic.
    pub fn claim_chat(&self, chat_id: &str, cwd: Option<&str>) -> Result<(), EngineError> {
        if self.inner.doc().chat(chat_id)?.is_some() {
            return Ok(());
        }
        let space_id = match cwd {
            Some(cwd) => Some(self.space_for_path(cwd)?),
            None => None,
        };
        self.inner.doc().upsert_chat(&Chat {
            id: chat_id.to_string(),
            device_id: self.inner.config.device_id.clone(),
            title: None,
            archived: false,
            cwd: cwd.map(str::to_string),
            branch: None,
            checkout_id: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: Utc::now(),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id,
            last_seen_at: None,
        })?;
        Ok(())
    }

    /// An own-device space whose path matches, else a freshly created one.
    fn space_for_path(&self, path: &str) -> Result<String, EngineError> {
        let device_id = &self.inner.config.device_id;
        if let Some(space) = self
            .inner
            .doc()
            .read_spaces()?
            .into_iter()
            .find(|s| s.device_id == *device_id && s.path == path)
        {
            return Ok(space.id);
        }
        let space = Space {
            id: crate::new_id(),
            device_id: device_id.clone(),
            path: path.to_string(),
            name: None,
            git_detected: false,
            git_checked_at: None,
            checkout_id: None,
            branch: None,
            created_at: Utc::now(),
        };
        self.inner.doc().upsert_space(&space)?;
        Ok(space.id)
    }

    /// The chat's configured harness/model row, when present (RunRequest harness
    /// selection; callers fall back to the engine default).
    pub fn chat_config(&self, chat_id: &str) -> Option<ChatConfig> {
        self.chat(chat_id).and_then(|c| c.config)
    }

    /// The whole chat row, when the workspace doc has one.
    pub fn chat(&self, chat_id: &str) -> Option<comet_proto::Chat> {
        match self.inner.doc().chat(chat_id) {
            Ok(chat) => chat,
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "workspace chat read failed");
                None
            }
        }
    }

    // ── host-side row writes ────────────────────────────────────────────────

    /// Sidebar freshness on message persist: preview = first 120 chars of the last
    /// message's text. Claims the row first so a pre-workspace chat gains one.
    pub fn note_message(&self, chat_id: &str, text: &str) {
        let preview: String = text.chars().take(120).collect();
        let result = self.claim_chat(chat_id, None).and_then(|_| {
            self.inner
                .doc()
                .set_chat_last_message(chat_id, &preview, Utc::now())
                .map_err(EngineError::from)
        });
        if let Err(err) = result {
            tracing::warn!(chat = %chat_id, error = %err, "workspace last-message write failed");
        }
    }

    /// Resume continuity: stamp the chat row with the harness-native session id
    /// of its latest run and the cwd it was created under (comet
    /// sessions.ts:1039). An empty `session_id`
    /// tombstones the row ("do not resume" after a rejected resume). Best-effort:
    /// a missing chat row (claim happens on first command) just returns.
    pub fn set_chat_harness_session(&self, chat_id: &str, session_id: &str, cwd: &str) {
        match self
            .inner
            .doc()
            .set_chat_harness_session(chat_id, session_id, cwd)
        {
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "workspace harness-session write failed");
            }
        }
    }

    /// The chat row's stored harness session `(session_id, cwd)`, if stamped.
    /// The empty-string tombstone passes through — callers must treat it as
    /// "explicitly no resume" (and must NOT fall back to older sources).
    pub fn chat_harness_session(&self, chat_id: &str) -> Option<(String, Option<String>)> {
        match self.inner.doc().chat(chat_id) {
            Ok(chat) => {
                let chat = chat?;
                let id = chat.harness_session_id?;
                Some((id, chat.harness_session_cwd))
            }
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "workspace chat read failed");
                None
            }
        }
    }

    /// Session-status row upsert (sessions engine transitions land here too, in
    /// addition to the local watch channel).
    pub fn record_session(&self, session: &Session) {
        if let Err(err) = self.inner.doc().upsert_session(session) {
            tracing::warn!(chat = %session.chat_id, error = %err, "workspace session write failed");
        }
    }

    // ── Mutate surface (LWW writes accepted from any device) ────────────────

    /// Create a chat *in a space*: the space fixes the host device and base cwd
    /// (`cwd` override = an isolated-worktree path). Fails when the space row is
    /// missing — the UI always creates chats from a picked space.
    pub fn create_chat(
        &self,
        chat_id: &str,
        space_id: &str,
        config: Option<ChatConfig>,
        cwd: Option<String>,
    ) -> Result<(), EngineError> {
        if self.inner.doc().chat(chat_id)?.is_some() {
            return Ok(()); // idempotent: optimistic client retries never duplicate
        }
        let Some(space) = self.inner.doc().space(space_id)? else {
            return Err(EngineError::Other(format!("no such space: {space_id}")));
        };
        self.inner.doc().upsert_chat(&Chat {
            id: chat_id.to_string(),
            device_id: space.device_id.clone(),
            title: None,
            archived: false,
            cwd: Some(cwd.unwrap_or_else(|| space.path.clone())),
            branch: None,
            checkout_id: None,
            config,
            last_message_preview: None,
            last_message_at: None,
            created_at: Utc::now(),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: Some(space.id),
            last_seen_at: None,
        })?;
        Ok(())
    }

    // ── spaces (Mutate surface + owner stamps) ──────────────────────────────

    /// Create a space (any device). Idempotent by id; a live duplicate of the
    /// same `(deviceId, path)` is a no-op backstop (the UI reuses via
    /// WatchSpaces). `git_detected` is seeded from the picker's FolderEntry;
    /// the owning device's SpacesSync re-verifies.
    pub fn create_space(
        &self,
        space_id: &str,
        device_id: &str,
        path: &str,
        name: Option<String>,
        git_detected: bool,
    ) -> Result<(), EngineError> {
        let spaces = self.inner.doc().read_spaces()?;
        if spaces
            .iter()
            .any(|s| s.id == space_id || (s.device_id == device_id && s.path == path))
        {
            return Ok(());
        }
        self.inner.doc().upsert_space(&Space {
            id: space_id.to_string(),
            device_id: device_id.to_string(),
            path: path.to_string(),
            name,
            git_detected,
            git_checked_at: None,
            checkout_id: None,
            branch: None,
            created_at: Utc::now(),
        })?;
        Ok(())
    }

    pub fn rename_space(&self, space_id: &str, name: Option<&str>) -> Result<bool, EngineError> {
        Ok(self.inner.doc().rename_space(space_id, name)?)
    }

    /// Hard-delete a space and its chats (doc cascade). The caller (rpc layer)
    /// tears down live runs / doc-host handles for the returned chat ids.
    pub fn delete_space(&self, space_id: &str) -> Result<DeletedSpace, EngineError> {
        Ok(self.inner.doc().delete_space(space_id)?)
    }

    /// Synced seen marker (any device; LWW + monotonic guard in the doc layer).
    pub fn mark_chat_seen(
        &self,
        chat_id: &str,
        at: chrono::DateTime<Utc>,
    ) -> Result<bool, EngineError> {
        Ok(self.inner.doc().set_chat_seen(chat_id, at)?)
    }

    /// Owner-only git stamp (SpacesSync). Refuses rows owned by another device.
    pub fn set_space_git(
        &self,
        space_id: &str,
        detected: bool,
        checkout_id: Option<&str>,
        branch: Option<&str>,
    ) -> Result<bool, EngineError> {
        match self.inner.doc().space(space_id)? {
            Some(space) if space.device_id == self.inner.config.device_id => {
                Ok(self.inner.doc().set_space_git(
                    space_id,
                    detected,
                    checkout_id,
                    branch,
                    Utc::now(),
                )?)
            }
            Some(space) => {
                tracing::warn!(
                    space = %space_id, owner = %space.device_id,
                    "refusing git stamp on space owned by another device"
                );
                Ok(false)
            }
            None => Ok(false),
        }
    }

    pub fn read_spaces(&self) -> Result<Vec<Space>, EngineError> {
        Ok(self.inner.doc().read_spaces()?)
    }

    pub fn rename_chat(&self, chat_id: &str, title: &str) -> Result<bool, EngineError> {
        Ok(self.inner.doc().rename_chat(chat_id, title)?)
    }

    /// Backdate a chat's activity timestamps (epoch ms). Returns false when
    /// the chat doesn't exist.
    pub fn set_chat_activity(
        &self,
        chat_id: &str,
        last_message_at: Option<i64>,
        created_at: Option<i64>,
    ) -> Result<bool, EngineError> {
        let Some(mut chat) = self.inner.doc().chat(chat_id)? else {
            return Ok(false);
        };
        if let Some(ms) = last_message_at {
            chat.last_message_at = chrono::DateTime::<Utc>::from_timestamp_millis(ms);
        }
        if let Some(ms) = created_at
            && let Some(at) = chrono::DateTime::<Utc>::from_timestamp_millis(ms)
        {
            chat.created_at = at;
        }
        self.inner.doc().upsert_chat(&chat)?;
        Ok(true)
    }

    /// Re-home a chat to another device (tooling/seeds; a future device
    /// migration flow will drive this). Returns false when the chat doesn't
    /// exist.
    pub fn set_chat_host(&self, chat_id: &str, device_id: &str) -> Result<bool, EngineError> {
        let Some(mut chat) = self.inner.doc().chat(chat_id)? else {
            return Ok(false);
        };
        chat.device_id = device_id.to_string();
        self.inner.doc().upsert_chat(&chat)?;
        Ok(true)
    }

    pub fn set_chat_archived(&self, chat_id: &str, archived: bool) -> Result<bool, EngineError> {
        Ok(self.inner.doc().set_chat_archived(chat_id, archived)?)
    }

    /// LWW full-config replace on the chat row (comet `SetChatConfig` — the
    /// composer's mid-session model/reasoning/options changes). Returns false
    /// when the chat doesn't exist.
    ///
    /// One field survives the replace: the chat's [`comet_proto::TurnLimits`]
    /// (gh#270). A full-config write is what a composer sends to change a
    /// *model*, and one sent by a viewport that predates the field — or by any
    /// of the several that build the struct from their own picker state —
    /// carries the default, which is "no guardrail". Letting that land would
    /// mean switching model in a dispatched chat quietly disarmed the run loop's
    /// only defence against a spinning agent. Nothing but a board dispatch ever
    /// sets these, and no surface can express clearing them, so an incoming
    /// `off` is an uninformed writer rather than a decision.
    pub fn set_chat_config(&self, chat_id: &str, config: &ChatConfig) -> Result<bool, EngineError> {
        let mut config = config.clone();
        if config.turn_limits.is_off()
            && let Some(existing) = self.chat_config(chat_id)
        {
            config.turn_limits = existing.turn_limits;
        }
        Ok(self.inner.doc().set_chat_config(chat_id, &config)?)
    }

    /// Tombstone: removes the chats (and session-status) row; the per-chat session
    /// doc remains untouched.
    pub fn delete_chat(&self, chat_id: &str) -> Result<bool, EngineError> {
        Ok(self.inner.doc().delete_chat(chat_id)?)
    }

    /// LWW rename from any device, written to both device docs so the new name
    /// reaches this user's other devices AND the rest of the org.
    pub fn rename_device(&self, device_id: &str, name: &str) -> Result<bool, EngineError> {
        let renamed = self.inner.doc().rename_device(device_id, name)?;
        self.inner.org_devices.rename_device(device_id, name);
        // The org registry may hold a row the private doc never did (another
        // user's device), so either write landing counts as a rename.
        Ok(renamed || self.inner.org_devices.device(device_id).is_some())
    }

    /// Every device the caller can address: this user's own rows merged with
    /// the org registry (gh#66).
    pub fn devices(&self) -> Vec<Device> {
        let own = self.inner.doc().read_devices().unwrap_or_else(|err| {
            tracing::warn!(error = %err, "workspace device read failed");
            Vec::new()
        });
        merge_devices(own, self.inner.org_devices.read_devices())
    }

    /// True once the org device registry room is live — the signal that this
    /// device is visible to the rest of the org.
    pub fn org_devices_connected(&self) -> bool {
        self.inner.org_devices.connected()
    }

    /// The org device registry this host owns (gh#66).
    pub fn org_devices(&self) -> OrgDevices {
        self.inner.org_devices.clone()
    }

    // ── git metadata (diff-sync host writes) ────────────────────────────────

    /// HEAD-watcher reconciliation: the branch checked out at the chat's cwd.
    pub fn set_chat_branch(&self, chat_id: &str, branch: &str) -> Result<bool, EngineError> {
        Ok(self.inner.doc().set_chat_branch(chat_id, branch)?)
    }

    /// Retarget a chat onto another folder (mid-session switch to an existing
    /// worktree). Resume is cwd-scoped — the next run there starts fresh.
    pub fn set_chat_cwd(&self, chat_id: &str, cwd: &str) -> Result<bool, EngineError> {
        Ok(self.inner.doc().set_chat_cwd(chat_id, cwd)?)
    }

    /// Canonical checkout identity for the chat's cwd (diff grouping key).
    pub fn set_chat_checkout(&self, chat_id: &str, checkout_id: &str) -> Result<bool, EngineError> {
        Ok(self.inner.doc().set_chat_checkout(chat_id, checkout_id)?)
    }

    // ── persistence / teardown ──────────────────────────────────────────────

    /// Persist the snapshot now (shutdown path; bypasses the debounce).
    pub fn flush(&self) {
        self.inner.save_snapshot();
    }

    /// Shutdown: stamp our `lastSeenAt` (the only periodic-ish map write besides
    /// boot) and flush the snapshot.
    pub fn shutdown(&self) {
        let now = Utc::now();
        if let Err(err) = self
            .inner
            .doc()
            .set_device_last_seen(&self.inner.config.device_id, now)
        {
            tracing::warn!(error = %err, "device lastSeenAt stamp failed");
        }
        self.inner
            .org_devices
            .set_device_last_seen(&self.inner.config.device_id, now);
        self.inner.save_snapshot();
    }
}

impl WorkspaceHostInner {
    fn publish(&self) {
        match self.doc().read_all() {
            Ok(mut state) => {
                // The device list is the union of this user's private rows and
                // the org registry — the org's devices are what a teammate has
                // to see to reach the box at all (gh#66).
                let mut devices = merge_devices(state.devices, self.org_devices.read_devices());
                self.overlay_presence(&mut devices);
                state.devices = Vec::new();
                // send_replace, NOT send: `watch::Sender::send` drops the value when
                // no receiver exists yet, so a stream subscribed later would start
                // from a stale snapshot (found the hard way by the e2e smoke).
                self.chats_tx.send_replace(state.chats);
                self.devices_tx.send_replace(devices);
                self.sessions_tx.send_replace(state.sessions);
                self.spaces_tx.send_replace(state.spaces);
            }
            Err(err) => {
                tracing::warn!(error = %err, "workspace read failed");
            }
        }
    }

    /// Fold this engine's presence belief into the device rows' `lastSeenAt`
    /// before publishing.
    ///
    /// The doc row is written on boot/shutdown ONLY (oplog hygiene), so without
    /// this overlay every device looks offline ~70s after its boot — and a
    /// genuinely dead host is indistinguishable from slow sync.
    ///
    /// A believed-connected device is stamped `now`, not the timestamp of the
    /// evidence. That is the semantic gh#145 turns on: the beat used to BE the
    /// timestamp, so freshness and belief were the same number. They no longer
    /// are — the room answers on socket-set changes and the relay is asked every
    /// 30s, neither on the 15s cadence a 70s window implies. Stamping `now`
    /// keeps the claim honest ("this engine believes it is connected, as of
    /// now") and every downstream window ([`comet_proto::view::host_presence`],
    /// the devices page) intact. A device that stops being believed simply stops
    /// being restamped and ages out of those windows on its own.
    ///
    /// Believed-connected peers also fire the peer-alive hook (dial-cooldown
    /// reset).
    fn overlay_presence(&self, devices: &mut [Device]) {
        let mut alive_peers: Vec<String> = Vec::new();
        {
            let belief = lock(&self.presence);
            let now = now_ms();
            let Some(at) = chrono::DateTime::<Utc>::from_timestamp_millis(now) else {
                return;
            };
            for device in devices.iter_mut() {
                if !belief.online(&device.id, now) {
                    continue;
                }
                if device.last_seen_at.is_none_or(|prev| prev < at) {
                    device.last_seen_at = Some(at);
                }
                if device.id != self.config.device_id {
                    alive_peers.push(device.id.clone());
                }
            }
        }
        if alive_peers.is_empty() {
            return;
        }
        let hook = lock(&self.peer_alive).clone();
        if let Some(hook) = hook {
            for id in &alive_peers {
                hook(id);
            }
        }
    }

    /// Fold the workspace room's latest `%EPH` answer into the belief. No live
    /// room handle changes nothing: our own pipe being down is not evidence
    /// about anyone else's device (gh#126).
    fn absorb_workspace_presence(&self) {
        let ids = lock(&self.room).as_ref().map(room_presence_ids);
        if let Some(ids) = ids {
            lock(&self.presence).room_answered(PresenceRoom::Workspace, ids);
        }
    }

    fn save_snapshot(&self) {
        match self.doc().export_snapshot() {
            Ok(bytes) => {
                if let Err(err) = self.store.save_snapshot(WORKSPACE_DOC_ID, &bytes) {
                    tracing::warn!(error = %err, "workspace snapshot save failed");
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "workspace snapshot export failed");
            }
        }
        self.org_devices.save_snapshot();
    }
}

/// Local live statuses win for this device's chats; every other device's rows come
/// from the workspace doc. Sorted by chat id (stable stream output).
fn merge_sessions(device_id: &str, rows: &[Session], local: &[Session]) -> Vec<Session> {
    let mut merged: std::collections::HashMap<String, Session> = rows
        .iter()
        .filter(|s| s.device_id != device_id)
        .map(|s| (s.chat_id.clone(), s.clone()))
        .collect();
    for session in local {
        merged.insert(session.chat_id.clone(), session.clone());
    }
    let mut list: Vec<Session> = merged.into_values().collect();
    list.sort_by(|a, b| a.chat_id.cmp(&b.chat_id));
    list
}

/// Background task: relay-verified presence, and since gh#145 the only periodic
/// network traffic presence generates at all.
///
/// Every [`RELAY_PROBE_INTERVAL_MS`], for every remote device the engine knows
/// of, ask its DeviceRoom whether the host socket is live
/// (`GET /device/{id}/status` → `hostConnected`). The DeviceRoom shares no
/// machinery with the sync rooms, so this cuts both ways:
///
/// - a positive answer keeps a device online when our own rooms are wedged or
///   dark, which is what stops a viewer's broken pipe reading as everyone
///   else's outage (gh#126);
/// - two consecutive negatives take a device offline even while a sync room is
///   still listing it, which is the ONLY way to catch a socket whose uplink
///   died silently — the runtime never closes such a socket, and with the 15s
///   heartbeat gone nothing else would ever look again.
///
/// It probes unconditionally now (it used to skip devices with fresh beats)
/// because there are no beats left to be fresh. The cost lands on the cheap
/// object on purpose: a `/status` answer is ~2ms of Durable Object duration
/// against the ~7s a single `%EPH` heartbeat was costing `SessionRoom`.
async fn relay_probe_task(weak: Weak<WorkspaceHostInner>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(RELAY_PROBE_INTERVAL_MS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await; // consume the immediate first tick
    let client = reqwest::Client::new();
    loop {
        tick.tick().await;
        let Some(inner) = weak.upgrade() else { return };
        let Some(edge) = inner.config.edge.clone() else {
            return;
        };
        let self_id = inner.config.device_id.clone();
        let targets: Vec<String> = {
            let Ok(own) = inner.doc().read_devices() else {
                continue;
            };
            // Every device either registry knows, plus anything the belief has
            // heard of and the docs have not caught up on yet. Teammates'
            // devices included: their presence and ours share the org room, so
            // a room pathology starves both alike.
            let mut targets = lock(&inner.presence).known();
            for device in merge_devices(own, inner.org_devices.read_devices()) {
                targets.insert(device.id);
            }
            targets.remove(&self_id);
            let mut targets: Vec<String> = targets.into_iter().collect();
            targets.sort(); // stable probe order; nothing depends on it but logs
            targets
        };
        drop(inner);
        if targets.is_empty() {
            continue;
        }
        let Some(bearer) = edge.bearer().await else {
            continue; // signed out
        };
        let mut moved = false;
        for device_id in targets {
            let url = format!(
                "{}/device/{}/status",
                edge.url.trim_end_matches('/'),
                device_id
            );
            let response = client
                .get(&url)
                .bearer_auth(&bearer)
                .timeout(RELAY_PROBE_TIMEOUT)
                .send()
                .await;
            // A probe that did not complete is not evidence of anything: our own
            // uplink, an expired token or an edge deploy must never be published
            // as someone else's device going dark.
            let Ok(response) = response else { continue };
            if !response.status().is_success() {
                continue;
            }
            let Ok(body) = response.json::<serde_json::Value>().await else {
                continue;
            };
            let connected = body
                .get("hostConnected")
                .and_then(serde_json::Value::as_bool);
            let Some(inner) = weak.upgrade() else { return };
            match connected {
                Some(true) => {
                    lock(&inner.presence).relay_alive(&device_id, now_ms());
                    tracing::debug!(device = %device_id, "presence: relay-verified alive");
                    moved = true;
                }
                Some(false) => {
                    lock(&inner.presence).relay_absent(&device_id);
                    tracing::debug!(device = %device_id, "presence: relay reports no live host");
                    moved = true;
                }
                None => {}
            }
        }
        if moved && let Some(inner) = weak.upgrade() {
            inner.publish();
        }
    }
}

/// Background task: reacts to doc changes (local commits and remote imports) by
/// re-publishing the watch channels and debouncing snapshots, and republishes
/// every [`PUBLISH_INTERVAL_MS`]. Holds only a weak handle so a dropped host
/// tears the task down.
async fn workspace_task(weak: Weak<WorkspaceHostInner>, mut changed_rx: watch::Receiver<u64>) {
    let mut republish =
        tokio::time::interval(std::time::Duration::from_millis(PUBLISH_INTERVAL_MS));
    republish.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    republish.tick().await; // consume the immediate first tick
    let mut save_deadline: Option<tokio::time::Instant> = None;
    loop {
        let sleep_until = save_deadline.unwrap_or_else(tokio::time::Instant::now);
        tokio::select! {
            changed = changed_rx.changed() => {
                if changed.is_err() {
                    break; // host (and its change sender) is gone
                }
                let Some(inner) = weak.upgrade() else { break };
                inner.publish();
                if save_deadline.is_none() {
                    save_deadline = Some(
                        tokio::time::Instant::now()
                            + std::time::Duration::from_millis(SNAPSHOT_DEBOUNCE_MS),
                    );
                }
            }
            _ = tokio::time::sleep_until(sleep_until), if save_deadline.is_some() => {
                save_deadline = None;
                let Some(inner) = weak.upgrade() else { break };
                inner.save_snapshot();
            }
            _ = republish.tick() => {
                let Some(inner) = weak.upgrade() else { break };
                // Local only — nothing leaves this process. The overlay stamps
                // `now` for believed-connected devices, so republishing is what
                // keeps a live badge from ageing out of the 70s window, and what
                // lets a device that stopped being believed age out of it.
                inner.publish();
            }
        }
    }
}

#[cfg(test)]
mod room_supervision_tests {
    use super::*;

    #[tokio::test]
    async fn malformed_protocol_parks_the_supervisor_instead_of_rebuilding() {
        let (events_tx, events_rx) = tokio::sync::broadcast::channel(4);
        let (_health_tx, health_rx) = tokio::sync::watch::channel(true);
        let on_event: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let supervision = tokio::spawn(async move {
            supervise_room("malformed-room", events_rx, health_rx, true, &on_event).await
        });

        events_tx
            .send(comet_sync::RoomEvent::ProtocolParked)
            .unwrap();
        let exit = tokio::time::timeout(std::time::Duration::from_secs(1), supervision)
            .await
            .expect("supervisor stops immediately")
            .expect("supervisor task");
        assert_eq!(exit, RoomExit::ProtocolParked);
    }
}
