//! DocHost — per-chat `SessionDoc` handles: snapshot persistence (debounced), edge room
//! sync (offline-tolerant), and the HOST-ONLY durable command executor.
//!
//! Pragmatic port of comet's `session-docs.ts` + the `main.ts` executor (spec:
//! feature-inventory §3.3, ARCHITECTURE §2 "command plane"):
//! - the doc IS the outbox: commands and user entries commit locally and sync whenever a
//!   room connection exists; the engine is fully functional with sync disabled;
//! - on every doc change (local commit or remote import) the handle re-emits the joined
//!   transcript to watchers, drains pending commands, and schedules a snapshot save;
//! - command drain: evaluate via `evaluate_command` (with the DocsStore processed
//!   ledger), mark processed BEFORE execute, execute through the sessions engine, then
//!   write the outcome status back into the doc as the sole outcome writer.
//!
//! Chat ownership is gated on the workspace doc (`chats[chat_id].deviceId`), with
//! claim-on-first-command for unknown chats. Queueing a command for a chat hosted on
//! another device POSTs a durable nudge to that device's room (§7 cold-chat delivery);
//! the host's relay receives it and warm-opens the doc, which drains the queue.
//!
//! The handle map is a CACHE, not a registry: [`DocHost::release_idle`] closes
//! chats nobody is using (gh#395). Every open chat costs a standing websocket to
//! the edge, and an insert-only map turned "chats this engine touched since
//! boot" into permanent edge load — 20 rooms ten minutes after a restart, 70% of
//! all edge traffic, and the multiplier behind the recurring Durable Objects
//! free-tier trips. Releasing is safe because [`DocHost::open`] rebuilds a
//! handle from the snapshot on demand, and a command for a released chat still
//! arrives: the nudge is what wakes a cold host, with or without a standing
//! socket.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::time::Duration;

use tokio::sync::watch;

use comet_doc::{
    COMMAND_DEFAULT_TTL_MS, CommandBasedOn, CommandDisposition, DocError, EvaluationContext,
    MessagePart, MessageRole, MessageStatus, SessionCommandEntry, SessionCommandPayload,
    SessionCommandStatus, SessionDoc, SessionMessageEntry, evaluate_command,
    join_continuation_entries,
};
use comet_proto::{HarnessId, UserInputAnswer, UserInputQuestion};
use comet_sync::{DocsStore, RoomClient};

use crate::sessions::{SessionsEngine, SteerOutcome};
use crate::workspace_host::WorkspaceHost;
use crate::{EngineError, new_id, now_ms};

/// Debounce window for local snapshot saves after a doc change.
const SNAPSHOT_DEBOUNCE_MS: u64 = 1_000;

/// How far back [`DocHost::warm_open_recent`] reaches (feature-inventory §3.3).
const WARM_OPEN_WINDOW_DAYS: i64 = 14;

/// Ceiling on boot-time warm opens — each one is a room socket and a live doc,
/// so a device hosting hundreds of chats must not open all of them at boot.
const WARM_OPEN_MAX: usize = 30;

/// How long a chat may sit unused before [`DocHost::release_idle`] closes it.
///
/// Short enough that a burst of chats does not leave a burst of sockets behind
/// for the rest of the day; long enough that a user reading one chat, answering
/// in another and coming back does not pay for a redial. A wrong release costs
/// a snapshot load and one dial — the same work a nudge already does.
const IDLE_RELEASE_MS: i64 = 5 * 60 * 1000;

/// Hard bound on simultaneously open chats, enforced least-recently-used-first
/// once the idle sweep has had its say. Above [`WARM_OPEN_MAX`] on purpose: the
/// boot warm-open is a deliberate burst and must not be trimmed the moment it
/// lands — the idle sweep is what gives those chats back once they have drained.
const MAX_OPEN_CHATS: usize = 32;

/// How often [`DocHost::spawn_idle_release`] sweeps.
const RELEASE_SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// Resident-memory estimate per compressed snapshot byte. Loro snapshots are
/// columnar+compressed; the in-memory doc plus mirror runs well above the blob
/// size. A rough multiplier is enough here — the byte budget is a safety
/// ceiling, the idle sweep and [`MAX_OPEN_CHATS`] do the day-to-day work.
const RESIDENT_BYTES_PER_SNAPSHOT_BYTE: usize = 6;

/// Floor per open doc (room socket buffers, tasks) regardless of content size.
const DOC_RESIDENT_FLOOR_BYTES: usize = 512 * 1024;

/// Edge connection config. The bearer is a **provider**, never a snapshot:
/// every room (re)connect and HTTP request re-reads it, so WorkOS access-token
/// refreshes (~1h expiry) take effect without an engine restart. Dev bearers
/// (which never expire) ride the same seam as a [`comet_rpc::StaticToken`].
#[derive(Clone)]
pub struct EdgeConfig {
    /// Edge base URL (`http(s)://…`); rewritten to `ws(s)` for the room socket.
    pub url: String,
    /// Fresh-bearer provider (the relay's `TokenSource`), consulted per
    /// connect/request. `None` from the provider = signed out.
    pub token: Arc<dyn comet_rpc::TokenSource>,
}

impl std::fmt::Debug for EdgeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EdgeConfig")
            .field("url", &self.url)
            .field("token", &"<provider>")
            .finish()
    }
}

impl EdgeConfig {
    pub fn new(url: impl Into<String>, token: Arc<dyn comet_rpc::TokenSource>) -> Self {
        Self {
            url: url.into(),
            token,
        }
    }

    /// Fixed bearer — dev mode and tests, where tokens never expire.
    pub fn with_static_token(url: impl Into<String>, token: impl Into<String>) -> Self {
        Self::new(url, Arc::new(comet_rpc::StaticToken(token.into())))
    }

    /// The current bearer, refreshed by the provider if stale. `None` = signed out.
    pub async fn bearer(&self) -> Option<String> {
        self.token.token().await
    }

    /// A per-dial room URL provider for `path` (e.g. `/session/{chatId}/ws`):
    /// the bearer is re-fetched before every connect, so reconnects after a
    /// token expiry present a fresh `?token=` instead of the boot-time one.
    pub fn room_url(&self, path: impl Into<String>) -> Arc<dyn comet_sync::UrlProvider> {
        self.room_url_as(path, None)
    }

    /// As [`Self::room_url`], naming the device this socket belongs to.
    ///
    /// That name is what lets the edge DERIVE presence from its socket set
    /// instead of being beaten awake every 15s to be told (gh#145). A socket
    /// with no device id contributes no presence — which is correct for a
    /// browser client, and is also what an engine older than gh#145 looks like.
    pub fn room_url_as(
        &self,
        path: impl Into<String>,
        device_id: Option<&str>,
    ) -> Arc<dyn comet_sync::UrlProvider> {
        let ws_base = self.url.replacen("http", "ws", 1);
        Arc::new(EdgeRoomUrl {
            base: format!("{}{}", ws_base.trim_end_matches('/'), path.into()),
            device_id: device_id.map(str::to_string),
            token: self.token.clone(),
        })
    }
}

struct EdgeRoomUrl {
    base: String,
    device_id: Option<String>,
    token: Arc<dyn comet_rpc::TokenSource>,
}

impl comet_sync::UrlProvider for EdgeRoomUrl {
    fn url(&self) -> futures::future::BoxFuture<'static, Result<String, comet_sync::SyncError>> {
        let token = self.token.clone();
        let base = self.base.clone();
        let device_id = self.device_id.clone();
        Box::pin(async move {
            let token = token.token().await.ok_or_else(|| {
                comet_sync::SyncError::Auth("no access token (signed out)".into())
            })?;
            // Device ids are `[A-Za-z0-9_-]` (the edge's own `ID_RE`, which
            // gates the routes that carry them), so nothing here needs escaping.
            let device = device_id
                .map(|id| format!("&deviceId={id}"))
                .unwrap_or_default();
            Ok(format!("{base}?token={token}{device}"))
        })
    }
}

#[derive(Debug, Clone)]
pub struct DocHostConfig {
    pub device_id: String,
    /// Harness for doc-command runs on chats without a workspace `config` row.
    pub default_harness: HarnessId,
    /// When present, each opened chat joins its edge session room. `None` = fully
    /// offline operation (local snapshots only).
    pub edge: Option<EdgeConfig>,
}

struct DocHostInner {
    store: Arc<DocsStore>,
    config: DocHostConfig,
    sessions: OnceLock<SessionsEngine>,
    workspace: OnceLock<WorkspaceHost>,
    handles: Mutex<HashMap<String, Arc<ChatDocHandle>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct DocHost {
    inner: Arc<DocHostInner>,
}

/// One open chat doc: the `SessionDoc`, its change plumbing, and the room client.
pub struct ChatDocHandle {
    chat_id: String,
    device_id: String,
    doc: Arc<SessionDoc>,
    messages_tx: watch::Sender<Vec<SessionMessageEntry>>,
    /// The session room, supervised by [`crate::workspace_host::spawn_room_join`].
    /// An `Arc` because the supervisor holds a `Weak` to it: dropping the handle
    /// is what ends the supervision (and, with it, the room client).
    room: Arc<Mutex<Option<RoomClient>>>,
    /// When this chat was last used: an [`DocHost::open`] (hit or miss) or a doc
    /// change (a local commit, or an import from the room). The idle sweep's
    /// clock — see [`DocHost::release_idle`].
    last_used: AtomicI64,
    /// True when the doc changed while nobody watched: the mirror rebuild is
    /// deferred to the next `watch_messages` attach instead of paid per commit.
    mirror_dirty: AtomicBool,
    /// Last known snapshot blob size — the release byte budget's input.
    snapshot_bytes: AtomicUsize,
    /// Doc subscription (drop = unsubscribe) — bumps the change watch on every commit.
    _sub: loro::Subscription,
}

impl ChatDocHandle {
    pub fn chat_id(&self) -> &str {
        &self.chat_id
    }

    /// Mark this chat used now, deferring its release.
    fn touch(&self) {
        self.last_used.store(now_ms(), Ordering::Relaxed);
    }

    fn last_used(&self) -> i64 {
        self.last_used.load(Ordering::Relaxed)
    }

    /// Is anyone streaming this chat's transcript right now? A watcher's stream
    /// is fed by `messages_tx` and ENDS when that sender drops, so releasing a
    /// watched chat would cut a live viewer's transcript off — the sweep pins on
    /// this rather than letting the UI discover it.
    fn watched(&self) -> bool {
        self.messages_tx.receiver_count() > 0
    }

    pub fn doc(&self) -> &SessionDoc {
        &self.doc
    }

    pub fn doc_arc(&self) -> Arc<SessionDoc> {
        self.doc.clone()
    }

    /// Joined transcript watch — re-sent on every doc change (WatchDocMessages).
    ///
    /// Attach-time refresh: the mirror is only maintained while watched, so a
    /// doc that changed unwatched materializes here, once, instead of on every
    /// commit it sat through in the background.
    pub fn watch_messages(&self) -> watch::Receiver<Vec<SessionMessageEntry>> {
        self.touch();
        // Subscribe BEFORE the dirty check: a commit racing this attach then
        // sees a live receiver and publishes, instead of re-marking dirty
        // after our refresh and leaving the new watcher a cleared mirror.
        let rx = self.messages_tx.subscribe();
        if self.mirror_dirty.load(Ordering::Acquire) {
            self.publish_messages();
        }
        rx
    }

    /// Is this chat's session room joined RIGHT NOW? (Not "do we hold a client"
    /// — see [`comet_sync::RoomClient::connected`], gh#116.)
    pub fn connected(&self) -> bool {
        lock(&self.room)
            .as_ref()
            .is_some_and(|client| client.connected())
    }

    /// Write a complete user message entry, idempotent by id (the client-minted message
    /// id — a re-executed command or optimistic echo never duplicates the entry).
    pub fn write_user_message(
        &self,
        message_id: &str,
        text: &str,
        created_at: i64,
    ) -> Result<(), DocError> {
        if self.doc.read_entries()?.iter().any(|e| e.id == message_id) {
            return Ok(());
        }
        self.doc.push_message(&SessionMessageEntry {
            id: message_id.to_string(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: text.to_string(),
            }],
            created_at,
            device_id: self.device_id.clone(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        })
    }

    /// Recovery sweep: stamp this device's abandoned `streaming` entries `aborted`, appending
    /// `note` as a visible error part so the transcript says WHY the turn
    /// ended (comet folded "Run interrupted by backend restart" the same
    /// way). Returns the stamped entries' `(id, created_at)` — recovery uses
    /// them for the resume-freshness check.
    pub fn mark_abandoned_streams(&self, note: &str) -> Result<Vec<(String, i64)>, DocError> {
        let mut stamped = Vec::new();
        for entry in self.doc.read_entries()? {
            if entry.role == MessageRole::Assistant
                && entry.status == Some(MessageStatus::Streaming)
                && entry.device_id == self.device_id
                && self
                    .doc
                    .set_message_status(&entry.id, MessageStatus::Aborted)?
            {
                let part_id = format!("{}-recovery", entry.id);
                if let Err(err) = self.doc.append_error_part(&entry.id, &part_id, note) {
                    tracing::warn!(chat = %self.chat_id, error = %err, "recovery note append failed");
                }
                stamped.push((entry.id.clone(), entry.created_at));
            }
        }
        if !stamped.is_empty() {
            self.publish_messages();
        }
        Ok(stamped)
    }

    fn publish_messages(&self) {
        self.mirror_dirty.store(false, Ordering::Release);
        match self.doc.read_entries() {
            Ok(entries) => {
                let joined = join_continuation_entries(entries);
                // send_replace: update the watch even with no subscribers yet, so a
                // late subscriber's first borrow sees the current transcript.
                self.messages_tx.send_replace(joined);
            }
            Err(err) => {
                tracing::warn!(chat = %self.chat_id, error = %err, "transcript read failed");
            }
        }
    }

    /// Per-commit publish path: unwatched docs just mark the mirror dirty —
    /// rebuilding a full transcript nobody reads was a per-tick cost on every
    /// open doc (and kept a second transcript copy hot).
    fn publish_messages_if_watched(&self) {
        if self.messages_tx.receiver_count() == 0 {
            self.mirror_dirty.store(true, Ordering::Release);
            // Shrink the stale mirror: watch_messages rebuilds on attach.
            self.messages_tx.send_replace(Vec::new());
        } else {
            self.publish_messages();
        }
    }

    /// Rough resident cost for the release byte budget.
    fn resident_estimate(&self) -> usize {
        (self.snapshot_bytes.load(Ordering::Relaxed) * RESIDENT_BYTES_PER_SNAPSHOT_BYTE)
            .max(DOC_RESIDENT_FLOOR_BYTES)
    }
}

impl DocHost {
    pub fn new(store: Arc<DocsStore>, config: DocHostConfig) -> Self {
        Self {
            inner: Arc::new(DocHostInner {
                store,
                config,
                sessions: OnceLock::new(),
                workspace: OnceLock::new(),
                handles: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Wire the sessions engine (engine assembly; see `SessionsEngine::set_doc_host`).
    pub fn set_sessions(&self, sessions: SessionsEngine) {
        let _ = self.inner.sessions.set(sessions);
        // Commands may already be pending in warm-opened docs.
        let handles: Vec<_> = lock(&self.inner.handles).values().cloned().collect();
        for handle in handles {
            let host = self.clone();
            tokio::spawn(async move { host.drain_commands(&handle).await });
        }
    }

    /// Wire the workspace host (engine assembly) — the source of chat-ownership rows.
    pub fn set_workspace(&self, workspace: WorkspaceHost) {
        let _ = self.inner.workspace.set(workspace);
    }

    /// The workspace host, once wired (tests may assemble a DocHost without one).
    pub fn workspace(&self) -> Option<&WorkspaceHost> {
        self.inner.workspace.get()
    }

    pub fn device_id(&self) -> &str {
        &self.inner.config.device_id
    }

    /// Chat ids with a live handle right now, sorted. Diagnostics — and the
    /// observable of [`Self::warm_open_recent`].
    pub fn open_chats(&self) -> Vec<String> {
        let mut ids: Vec<String> = lock(&self.inner.handles).keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Is this engine configured to sync chats to an edge at all?
    pub fn edge_enabled(&self) -> bool {
        self.inner.config.edge.is_some()
    }

    /// `(open chat docs, of which hold a LIVE session room)` — the chat half of
    /// [`comet_proto::EdgeHealth`]. Every open chat is meant to hold a room, so
    /// a gap between the two numbers is rooms that need to come back.
    ///
    /// A chat [`Self::release_idle`] closed is in neither number, which is the
    /// honest answer: it has no socket because it is not meant to have one. The
    /// gap keeps meaning what it meant — rooms that are down.
    pub fn room_census(&self) -> (usize, usize) {
        if !self.edge_enabled() {
            return (0, 0);
        }
        let handles: Vec<_> = lock(&self.inner.handles).values().cloned().collect();
        let live = handles.iter().filter(|h| h.connected()).count();
        (handles.len(), live)
    }

    /// Open (or return) the chat's doc handle: load the local snapshot (or init fresh),
    /// start the change-driven task, and join the edge room when configured.
    ///
    /// A chat [`Self::release_idle`] closed re-opens here transparently: the
    /// snapshot is the doc, so the caller cannot tell a rebuilt handle from a
    /// cached one — only the room has to be dialled again.
    pub fn open(&self, chat_id: &str) -> Result<Arc<ChatDocHandle>, EngineError> {
        if let Some(handle) = lock(&self.inner.handles).get(chat_id) {
            handle.touch();
            return Ok(handle.clone());
        }
        let mut snapshot_len = 0usize;
        let doc = match self.inner.store.load_snapshot(chat_id)? {
            Some(bytes) => {
                snapshot_len = bytes.len();
                let raw = loro::LoroDoc::new();
                raw.import(&bytes)
                    .map_err(|e| EngineError::Other(format!("snapshot import failed: {e}")))?;
                SessionDoc::from_doc(raw)
            }
            None => SessionDoc::init(chat_id)?,
        };
        let doc = Arc::new(doc);

        let (changed_tx, changed_rx) = watch::channel(0u64);
        let sub = doc.doc().subscribe_root(Arc::new(move |_diff| {
            changed_tx.send_modify(|v| *v = v.wrapping_add(1));
        }));
        // The mirror starts dirty and empty: many opens (command queueing,
        // drains, nudges) never watch the transcript, and the first
        // watch_messages attach materializes it on demand.
        let (messages_tx, _) = watch::channel(Vec::new());

        let handle = Arc::new(ChatDocHandle {
            chat_id: chat_id.to_string(),
            device_id: self.inner.config.device_id.clone(),
            doc: doc.clone(),
            messages_tx,
            room: Arc::new(Mutex::new(None)),
            last_used: AtomicI64::new(now_ms()),
            mirror_dirty: AtomicBool::new(true),
            snapshot_bytes: AtomicUsize::new(snapshot_len),
            _sub: sub,
        });
        {
            let mut handles = lock(&self.inner.handles);
            if let Some(existing) = handles.get(chat_id) {
                existing.touch();
                return Ok(existing.clone()); // racing open — keep the first
            }
            handles.insert(chat_id.to_string(), handle.clone());
        }
        // Say in the doc itself who hosts this chat, so a device that opens it
        // WITHOUT a workspace row (a teammate, on a chat shared into the org)
        // knows not to execute its commands (gh#66).
        self.stamp_host(&handle);

        // Edge room join — offline-tolerant AND supervised (gh#116). A one-shot
        // join left an open chat permanently roomless whenever the first dial
        // lost a race with an edge deploy or a token refresh: the handle stays
        // in `handles`, so re-opening it (a nudge, a watcher) hands back the
        // same roomless handle forever. The supervisor retries the first join
        // and rebuilds a client that stops reconnecting.
        if let Some(edge) = &self.inner.config.edge {
            crate::workspace_host::spawn_room_join(
                edge.room_url_as(
                    format!("/session/{chat_id}/ws"),
                    Some(&self.inner.config.device_id),
                ),
                chat_id.to_string(),
                doc.doc().clone(),
                Arc::downgrade(&handle.room),
                Arc::new(|| {}),
            );
        }

        tokio::spawn(chat_task(self.clone(), Arc::downgrade(&handle), changed_rx));
        Ok(handle)
    }

    /// Boot-time warm-open of recent chats (feature-inventory §3.3): open every
    /// chat THIS device hosts that has been active within [`WARM_OPEN_WINDOW_DAYS`],
    /// newest first, capped at [`WARM_OPEN_MAX`]. Returns how many were opened.
    ///
    /// Opening a doc joins its room and runs the command drain, so this is what
    /// makes a command queued while the device was down actually execute at
    /// boot. Until now cold chats relied on the nudge alone — and a nudge fired
    /// at a device that is off is delivered on rejoin at best and lost at
    /// worst, which on an unattended box means a queued run that simply never
    /// happens. This closes that on the one event that reliably follows a
    /// restart: the restart itself.
    ///
    /// Best-effort throughout: an unreadable row or a failed open is logged and
    /// skipped, never fatal — the engine boots either way.
    pub fn warm_open_recent(&self) -> usize {
        let Some(workspace) = self.workspace() else {
            return 0; // bare-DocHost tests: no ownership rows to select on
        };
        let chats = match workspace.doc().read_chats() {
            Ok(chats) => chats,
            Err(err) => {
                tracing::warn!(error = %err, "warm-open: workspace chat read failed");
                return 0;
            }
        };
        let cutoff = chrono::Utc::now() - chrono::Duration::days(WARM_OPEN_WINDOW_DAYS);
        let device_id = &self.inner.config.device_id;
        // `last_message_at` is only stamped once a turn has run; fall back to
        // `created_at` so a chat created remotely with a run already queued —
        // exactly the cold-delivery case — is inside the window too.
        let mut recent: Vec<(chrono::DateTime<chrono::Utc>, String)> = chats
            .into_iter()
            .filter(|chat| &chat.device_id == device_id && !chat.archived)
            .map(|chat| (chat.last_message_at.unwrap_or(chat.created_at), chat.id))
            .filter(|(at, _)| *at >= cutoff)
            .collect();
        recent.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let total = recent.len();
        recent.truncate(WARM_OPEN_MAX);
        let mut opened = 0usize;
        for (_, chat_id) in &recent {
            match self.open(chat_id) {
                Ok(_) => opened += 1,
                Err(err) => {
                    tracing::warn!(chat = %chat_id, error = %err, "warm-open failed")
                }
            }
        }
        if total > 0 {
            tracing::info!(
                opened,
                skipped = total.saturating_sub(recent.len()),
                "warm-opened recent chats"
            );
        }
        opened
    }

    /// Start the idle-release sweep: every [`RELEASE_SWEEP_INTERVAL`], hand back
    /// the chats nobody is using (gh#395). Needs a runtime — a bare synchronous
    /// assembly (unit tests) skips it rather than panicking.
    ///
    /// The task holds a `Weak` to the host, so a dropped engine ends the sweep
    /// instead of keeping its docs and sockets alive for the process's life.
    pub fn spawn_idle_release(&self) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(RELEASE_SWEEP_INTERVAL);
            ticker.tick().await; // the first tick is immediate
            loop {
                ticker.tick().await;
                let Some(inner) = weak.upgrade() else { return };
                DocHost { inner }.release_idle();
            }
        });
    }

    /// Close the chats nobody is using and return how many were released.
    ///
    /// "Using" is asked of the things that would actually break: a live watcher
    /// (its transcript stream dies with the sender), a live run (it streams
    /// through a doc handle of its own, so the cache cannot see it), and any
    /// caller currently holding the handle. Everything else is released once it
    /// has been idle for [`IDLE_RELEASE_MS`], and the least recently used go
    /// early when more than [`MAX_OPEN_CHATS`] are open.
    pub fn release_idle(&self) -> usize {
        self.release_idle_at(
            now_ms(),
            IDLE_RELEASE_MS,
            MAX_OPEN_CHATS,
            comet_doc::DOC_LRU_BYTE_BUDGET,
        )
    }

    /// [`Self::release_idle`] with the clock and the bounds passed in — the seam
    /// the policy tests drive, so they need neither a five-minute wait nor 32
    /// chats.
    fn release_idle_at(&self, now: i64, idle_ms: i64, max_open: usize, byte_budget: usize) -> usize {
        // Pass 1, under the lock: everything the cache itself knows. No call out
        // to another service from here — a dispatch on another thread walks
        // sessions → doc host, and this lock must never be held facing back.
        let mut candidates: Vec<RoomCandidate> = {
            let handles = lock(&self.inner.handles);
            handles
                .iter()
                .map(|(chat_id, handle)| RoomCandidate {
                    chat_id: chat_id.clone(),
                    last_used: handle.last_used(),
                    // `strong_count == 1` is the map's own reference: anything
                    // more is a caller mid-write holding the handle.
                    pinned: handle.watched() || Arc::strong_count(handle) > 1,
                    resident: handle.resident_estimate(),
                })
                .collect()
        };
        // Pass 2, lock released: the runs the cache cannot see.
        for candidate in &mut candidates {
            candidate.pinned = candidate.pinned || self.chat_is_busy(&candidate.chat_id);
        }
        let releasing = rooms_to_release(candidates, now, idle_ms, max_open, byte_budget);
        if releasing.is_empty() {
            return 0;
        }
        // Pass 3: take them out, re-checking under the lock — an `open()`
        // between the passes either handed the handle out (strong count) or
        // moved the clock, and either way this chat is in use again.
        let mut released = Vec::new();
        {
            let mut handles = lock(&self.inner.handles);
            for (chat_id, decided_on) in releasing {
                let stale = handles.get(&chat_id).is_some_and(|handle| {
                    handle.last_used() != decided_on
                        || handle.watched()
                        || Arc::strong_count(handle) > 1
                });
                if stale {
                    continue;
                }
                if let Some(handle) = handles.remove(&chat_id) {
                    released.push(handle);
                }
            }
        }
        let count = released.len();
        for handle in released {
            let chat_id = handle.chat_id.clone();
            let doc = handle.doc_arc();
            tracing::debug!(chat = %chat_id, "releasing idle chat room");
            // Drop BEFORE the save, not after: dropping the handle is what ends
            // the room supervision (it holds only a `Weak` to the room slot) and
            // the chat task, so by the time the snapshot is exported nothing can
            // still be importing changes into the doc behind it.
            drop(handle);
            self.save_doc(&chat_id, &doc);
        }
        if count > 0 {
            tracing::info!(
                released = count,
                open = lock(&self.inner.handles).len(),
                "released idle chat rooms"
            );
        }
        count
    }

    /// Drop a chat's doc unconditionally and delete its local snapshot — the
    /// chat is gone (DeleteChat / DeleteSpace cascade). Watchers see the
    /// stream end; a racing writer keeps its orphaned doc until the run ends.
    pub fn purge_chat(&self, chat_id: &str) {
        let removed = lock(&self.inner.handles).remove(chat_id);
        drop(removed);
        if let Err(err) = self.inner.store.delete_snapshot(chat_id) {
            tracing::warn!(chat = %chat_id, error = %err, "snapshot delete failed");
        }
    }

    /// Is a run live on this chat? Unwired sessions (bare-DocHost tests) means
    /// nothing is running.
    fn chat_is_busy(&self, chat_id: &str) -> bool {
        self.inner
            .sessions
            .get()
            .is_some_and(|sessions| sessions.chat_is_busy(chat_id))
    }

    /// Composer path: append an immutable pending command entry (rule 1). Durable by
    /// construction — the change subscription kicks the drain, so a local host executes
    /// immediately and an offline doc simply holds the entry until it syncs.
    pub fn queue_command(
        &self,
        chat_id: &str,
        payload: SessionCommandPayload,
    ) -> Result<String, EngineError> {
        let handle = self.open(chat_id)?;
        let id = new_id();
        let now = now_ms();
        let based_on = handle.doc.read_entries()?.last().map(|m| CommandBasedOn {
            turn_id: Some(m.id.clone()),
            frontier: None,
        });
        handle.doc.queue_command(&SessionCommandEntry {
            id: id.clone(),
            payload,
            issued_by: self.inner.config.device_id.clone(),
            issued_at: now,
            based_on,
            expires_at: Some(now + COMMAND_DEFAULT_TTL_MS),
            status: SessionCommandStatus::Pending,
            resolution: None,
        })?;
        // §7 durable delivery: when another device hosts this chat, nudge its device
        // room so a cold host opens the doc and drains the queue. Fire-and-forget —
        // the command is durable in the doc either way (a host that opens the chat
        // for any other reason still executes it).
        self.nudge_remote_host(chat_id);
        Ok(id)
    }

    /// The device hosting `chat_id`, when anything says: the session doc's own
    /// stamp first (it travels with a chat shared into the org — the only
    /// source a teammate has), then this user's workspace row. `None` = nobody
    /// has claimed it.
    fn host_device(&self, chat_id: &str) -> Option<String> {
        let stamped = lock(&self.inner.handles)
            .get(chat_id)
            .and_then(|handle| handle.doc.host_device_id());
        if stamped.is_some() {
            return stamped;
        }
        match self.workspace()?.doc().chat(chat_id) {
            Ok(chat) => chat.map(|c| c.device_id),
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "workspace chat read failed");
                None
            }
        }
    }

    /// Make a chat visible to the whole org at the edge (`POST /share/{chatId}`,
    /// gh#66) — how a board dispatch becomes work a teammate can open and steer
    /// rather than a chat only the box's owner may join. Owner-gated at the
    /// edge; idempotent; best-effort (offline/edge-less engines skip silently,
    /// and the chat stays private until a later dispatch re-shares it).
    pub fn share_chat(&self, chat_id: &str) {
        // Sharing is only ever done by the chat's host (the edge enforces that
        // too), so stamp the doc before anyone else can open it: a teammate who
        // synced a shared chat with no `hostDeviceId` would read "unclaimed" and
        // execute its commands themselves.
        match self.open(chat_id) {
            Ok(handle) => self.stamp_host(&handle),
            Err(err) => tracing::warn!(chat = %chat_id, error = %err, "share: open failed"),
        }
        let Some(edge) = self.inner.config.edge.clone() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let url = format!("{}/share/{}", edge.url.trim_end_matches('/'), chat_id);
        let chat = chat_id.to_string();
        runtime.spawn(async move {
            let Some(bearer) = edge.bearer().await else {
                tracing::warn!(chat = %chat, "share skipped: signed out");
                return;
            };
            let send = reqwest::Client::new()
                .post(&url)
                .bearer_auth(&bearer)
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await;
            match send {
                Ok(res) if res.status().is_success() => {
                    tracing::info!(chat = %chat, "chat shared with the org");
                }
                Ok(res) => tracing::warn!(chat = %chat, status = res.status().as_u16(),
                    "chat share rejected"),
                Err(err) => tracing::warn!(chat = %chat, error = %err, "chat share failed"),
            }
        });
    }

    /// POST `{edge}/device/{host}/nudge {chatId}` when another device hosts this
    /// chat. Best-effort: offline/edge-less engines skip silently.
    fn nudge_remote_host(&self, chat_id: &str) {
        let Some(edge) = self.inner.config.edge.clone() else {
            return;
        };
        let Some(host_device) = self.host_device(chat_id) else {
            // Unclaimed chat: whoever drains first claims it — nobody to nudge.
            return;
        };
        if host_device == self.inner.config.device_id {
            return;
        }
        // Only meaningful inside a runtime (RPC handlers, executors); bare sync
        // callers (unit tests) skip rather than panic.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let url = format!(
            "{}/device/{}/nudge",
            edge.url.trim_end_matches('/'),
            host_device
        );
        let chat = chat_id.to_string();
        runtime.spawn(async move {
            // Fresh bearer per request — never the boot-time snapshot.
            let Some(bearer) = edge.bearer().await else {
                tracing::warn!(chat = %chat, "nudge skipped: signed out");
                return;
            };
            let send = reqwest::Client::new()
                .post(&url)
                .bearer_auth(&bearer)
                .json(&serde_json::json!({ "chatId": chat }))
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await;
            match send {
                Ok(res) if res.status().is_success() => {
                    tracing::info!(chat = %chat, device = %host_device, "host nudged");
                }
                Ok(res) => tracing::warn!(chat = %chat, device = %host_device,
                    status = res.status().as_u16(), "nudge rejected"),
                Err(err) => {
                    tracing::warn!(chat = %chat, error = %err, "nudge failed (best-effort)")
                }
            }
        });
    }

    /// §2.2 writer discipline: we host a chat iff its workspace row's `deviceId` is
    /// ours; a chat with no row is claimable (claim-on-first-command). Without a
    /// wired workspace host (bare-DocHost tests) every open chat is ours — M2's
    /// behavior, now the degenerate case.
    ///
    /// The session doc's own `hostDeviceId` outranks all of that when it is
    /// stamped (gh#66). The workspace doc is PER-USER: a chat the box shared
    /// into the org has no row on a teammate's laptop, and "no row" reads as
    /// "claimable, so mine to execute" — which would run the box's work a
    /// second time, in a cwd that does not exist there. The stamp travels with
    /// the chat, so the answer is the same everywhere the chat is.
    ///
    /// Asked of the handle the caller already holds, never of the cache: the
    /// drain's answer must not depend on the chat still being cached (gh#395).
    fn is_host(&self, handle: &ChatDocHandle) -> bool {
        if let Some(host) = handle.doc.host_device_id() {
            return host == self.inner.config.device_id;
        }
        self.workspace()
            .is_none_or(|ws| ws.is_host(&handle.chat_id))
    }

    /// Record this device as the chat's host in the session doc, when the
    /// workspace says we are. Idempotent (a no-op once stamped), so warm-open
    /// and every later drain cost nothing.
    fn stamp_host(&self, handle: &ChatDocHandle) {
        let hosted_here = self
            .workspace()
            .and_then(|ws| ws.doc().chat(&handle.chat_id).ok().flatten())
            .is_some_and(|chat| chat.device_id == self.inner.config.device_id);
        if !hosted_here {
            return;
        }
        if let Err(err) = handle.doc.set_host_device_id(&self.inner.config.device_id) {
            tracing::warn!(chat = %handle.chat_id, error = %err, "host stamp failed");
        }
    }

    /// Chat-config harness when the workspace row carries one, else the default.
    pub(crate) fn harness_for(&self, chat_id: &str) -> HarnessId {
        self.workspace()
            .and_then(|ws| ws.chat_config(chat_id))
            .map(|config| config.harness)
            .unwrap_or(self.inner.config.default_harness)
    }

    /// Drain pending commands (host-only): evaluate → mark processed BEFORE execute →
    /// execute → write the outcome as the sole outcome writer.
    pub async fn drain_commands(&self, handle: &Arc<ChatDocHandle>) {
        let Some(sessions) = self.inner.sessions.get() else {
            return; // executor not wired yet; the set_sessions kick re-drains
        };
        if !self.is_host(handle) {
            return;
        }
        // Entries this pass decided to leave alone (processed dedupe hits).
        let mut skipped: HashSet<String> = HashSet::new();
        loop {
            let commands = match handle.doc.read_commands() {
                Ok(commands) => commands,
                Err(err) => {
                    tracing::warn!(chat = %handle.chat_id, error = %err, "command read failed");
                    return;
                }
            };
            let is_processed = |id: &str| self.inner.store.is_processed(id).unwrap_or(false);
            let Some(entry) = commands
                .iter()
                .find(|c| {
                    c.status == SessionCommandStatus::Pending
                        && !skipped.contains(&c.id)
                        && !is_processed(&c.id)
                })
                .cloned()
            else {
                return;
            };
            let messages = handle.doc.read_entries().unwrap_or_default();
            let current_turn_id = messages.last().map(|m| m.id.clone());
            let turn_is_past = |turn_id: &str| messages.iter().any(|m| m.id == turn_id);
            let disposition = evaluate_command(
                &entry,
                &EvaluationContext {
                    is_processed: &is_processed,
                    now_ms: now_ms(),
                    entries: &commands,
                    current_turn_id: current_turn_id.as_deref(),
                    turn_is_past: &turn_is_past,
                },
            );
            // Mark BEFORE executing: a crash mid-execution must never double-run a
            // command whose side effect may already have happened.
            if let Err(err) = self.inner.store.mark_processed(&entry.id) {
                tracing::error!(chat = %handle.chat_id, error = %err, "processed-ledger write failed; halting drain");
                return;
            }
            match disposition {
                CommandDisposition::Skip => {
                    skipped.insert(entry.id.clone());
                }
                CommandDisposition::Expired => {
                    self.resolve_command(handle, &entry.id, SessionCommandStatus::Expired, None);
                }
                CommandDisposition::Superseded => {
                    self.resolve_command(handle, &entry.id, SessionCommandStatus::Superseded, None);
                }
                CommandDisposition::Execute => {
                    let (status, resolution) = match self.execute(sessions, handle, &entry).await {
                        Ok(outcome) => outcome,
                        Err(err) => (SessionCommandStatus::Rejected, Some(err.to_string())),
                    };
                    self.resolve_command(handle, &entry.id, status, resolution.as_deref());
                }
            }
        }
    }

    /// Host-only outcome write (ledger rule 2).
    fn resolve_command(
        &self,
        handle: &ChatDocHandle,
        command_id: &str,
        status: SessionCommandStatus,
        resolution: Option<&str>,
    ) {
        if let Err(err) = handle
            .doc
            .set_command_status(command_id, status, resolution)
        {
            tracing::warn!(
                chat = %handle.chat_id,
                command = %command_id,
                error = %err,
                "command outcome write failed"
            );
        }
    }

    async fn execute(
        &self,
        sessions: &SessionsEngine,
        handle: &Arc<ChatDocHandle>,
        entry: &SessionCommandEntry,
    ) -> Result<(SessionCommandStatus, Option<String>), EngineError> {
        let chat_id = &handle.chat_id;
        match &entry.payload {
            SessionCommandPayload::Run {
                request,
                message_id,
            } => {
                // Claim-on-first-command: a run for a chat with no workspace row
                // creates the row under our device id (we are about to host it).
                if let Some(ws) = self.workspace() {
                    ws.claim_chat(chat_id, Some(&request.cwd))?;
                }
                self.stamp_host(handle);
                let mut request = request.clone();
                // An existing chat runs where its row says, not where the
                // sender guessed. Identical for every composer on a device that
                // has the row (it sends the row's cwd back), and the difference
                // that matters for a chat shared into the org: a teammate has
                // no row, so their send would otherwise arrive with a
                // placeholder cwd and run the box's work in the wrong folder.
                if let Some(cwd) = self
                    .workspace()
                    .and_then(|ws| ws.doc().chat(chat_id).ok().flatten())
                    .and_then(|chat| chat.cwd)
                    .filter(|cwd| !cwd.is_empty())
                {
                    request.cwd = cwd;
                }
                let harness = self.harness_for(chat_id);
                sessions
                    .dispatch(chat_id, harness, request, Some(message_id.clone()))
                    .await?;
                Ok((SessionCommandStatus::Applied, None))
            }
            SessionCommandPayload::Steer { prompt, message_id } => {
                match sessions.steer(chat_id, prompt, message_id.clone()).await? {
                    SteerOutcome::Accepted => Ok((SessionCommandStatus::Applied, None)),
                    SteerOutcome::NotSteerable => {
                        // No live steerable run: the durable command still delivers —
                        // run it as the next turn (comet's fallback, executor-side).
                        // After an engine restart `last_request` is empty too, so
                        // rebuild the run config from the chat's workspace row
                        // (comet derived dispatch config from the chat row the
                        // same way — sessions.ts:601-620); dispatch's engine-owned
                        // resume then reattaches the prior harness conversation.
                        let request = sessions
                            .last_request(chat_id)
                            .or_else(|| self.request_from_chat_row(chat_id, prompt));
                        let Some(mut request) = request else {
                            return Ok((
                                SessionCommandStatus::Rejected,
                                Some("no live run and no prior run config".into()),
                            ));
                        };
                        request.prompt = prompt.clone();
                        request.resume = None; // dispatch re-derives the harness session
                        // A reused config must not re-inline the PREVIOUS
                        // turn's images; this steer's own refs (if any) already
                        // ride the prompt text.
                        request.attachments = Vec::new();
                        sessions
                            .dispatch(
                                chat_id,
                                self.harness_for(chat_id),
                                request,
                                message_id.clone(),
                            )
                            .await?;
                        Ok((
                            SessionCommandStatus::Applied,
                            Some("queued as new turn".into()),
                        ))
                    }
                }
            }
            SessionCommandPayload::Interrupt {} => {
                sessions.interrupt(chat_id).await?;
                Ok((SessionCommandStatus::Applied, None))
            }
            SessionCommandPayload::RespondInput {
                request_id,
                answers,
            } => {
                if sessions.respond_input(chat_id, request_id, answers.clone())? {
                    return Ok((SessionCommandStatus::Applied, None));
                }
                // No live resolver. Only a request id the doc shows as an
                // OPEN question on a SETTLED entry gets the orphan fallback:
                // a mismatched or already-resolved id is a stale/buggy answer
                // and must still reject, and a still-streaming entry's
                // question belongs to the live run (a just-consumed resolver
                // racing a second answer must not spawn a duplicate turn).
                let questions = handle.doc.read_entries().ok().and_then(|entries| {
                    entries
                        .iter()
                        .rev()
                        .filter(|e| e.status != Some(MessageStatus::Streaming))
                        .find_map(|e| {
                            e.parts.iter().find_map(|p| match p {
                                MessagePart::Input {
                                    request_id: rid,
                                    questions,
                                    resolved: false,
                                    ..
                                } if rid == request_id => Some(questions.clone()),
                                _ => None,
                            })
                        })
                });
                let Some(questions) = questions else {
                    return Ok((
                        SessionCommandStatus::Rejected,
                        Some("no pending input request".into()),
                    ));
                };
                // The run died under the question (engine restart, crash).
                // The question is still open in the doc and the command is
                // durable, so honor it anyway — stamp the part resolved and
                // deliver the answers as the next (resumed) turn, the same
                // fallback a dead-run steer takes. The question UI stays up
                // until the user answers (user requirement); this is what
                // makes that answer still WORK.
                let request = sessions
                    .last_request(chat_id)
                    .or_else(|| self.request_from_chat_row(chat_id, ""));
                let Some(mut request) = request else {
                    return Ok((
                        SessionCommandStatus::Rejected,
                        Some("no pending input request and no prior run config".into()),
                    ));
                };
                request.prompt = respond_input_prompt(&questions, answers);
                request.resume = None; // dispatch re-derives the harness session
                request.attachments = Vec::new();
                if let Err(err) = handle.doc.resolve_input(request_id) {
                    tracing::warn!(chat = %chat_id, request = %request_id, error = %err,
                        "orphaned input resolve failed");
                }
                sessions
                    .dispatch(chat_id, self.harness_for(chat_id), request, None)
                    .await?;
                Ok((
                    SessionCommandStatus::Applied,
                    Some("answered as new turn".into()),
                ))
            }
        }
    }

    /// A steer-turned-run with no in-process `last_request` (engine restarted
    /// since the last turn): rebuild the run config from the chat's workspace
    /// row — cwd from the row, model/reasoning/options/sandbox from its config
    /// (composer defaults otherwise). `None` without a workspace host or row.
    // (Also the RespondInput dead-run fallback's config source.)
    pub(crate) fn request_from_chat_row(
        &self,
        chat_id: &str,
        prompt: &str,
    ) -> Option<comet_proto::RunRequest> {
        let workspace = self.workspace()?;
        let chat = match workspace.doc().chat(chat_id) {
            Ok(chat) => chat?,
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "workspace chat read failed");
                return None;
            }
        };
        let config = chat.config;
        Some(comet_proto::RunRequest {
            prompt: prompt.to_string(),
            model: config.as_ref().and_then(|c| c.model.clone()),
            reasoning: config.as_ref().and_then(|c| c.reasoning),
            model_options: config
                .as_ref()
                .map(|c| c.model_options.clone())
                .unwrap_or_default(),
            cwd: chat.cwd.unwrap_or_default(),
            sandbox: config
                .as_ref()
                .map(|c| c.sandbox)
                .unwrap_or(comet_proto::SandboxLevel::WorkspaceWrite),
            auto_approve: false,
            attachments: Vec::new(),
            resume: None,
        })
    }

    fn save_snapshot(&self, handle: &ChatDocHandle) {
        if let Some(bytes) = self.save_doc(&handle.chat_id, &handle.doc) {
            handle.snapshot_bytes.store(bytes, Ordering::Relaxed);
        }
    }

    /// [`Self::save_snapshot`] against the doc alone — the release path, which
    /// has let go of the handle (and with it the room) before it exports.
    /// Returns the exported blob size (the byte budget's freshest input).
    fn save_doc(&self, chat_id: &str, doc: &SessionDoc) -> Option<usize> {
        match doc.export_snapshot() {
            Ok(bytes) => {
                if let Err(err) = self.inner.store.save_snapshot(chat_id, &bytes) {
                    tracing::warn!(chat = %chat_id, error = %err, "snapshot save failed");
                }
                Some(bytes.len())
            }
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "snapshot export failed");
                None
            }
        }
    }

    /// Persist every open doc now (shutdown path; bypasses the debounce).
    pub fn flush_all(&self) {
        let handles: Vec<_> = lock(&self.inner.handles).values().cloned().collect();
        for handle in handles {
            self.save_snapshot(&handle);
        }
    }
}

/// One open chat, as the release policy sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RoomCandidate {
    chat_id: String,
    /// [`ChatDocHandle::last_used`] at the moment the sweep looked.
    last_used: i64,
    /// Something outside the cache is using this chat right now — a watcher, a
    /// live run, a caller holding the handle. Never released, at any age.
    pinned: bool,
    /// [`ChatDocHandle::resident_estimate`] — the byte-budget rule's input.
    resident: usize,
}

/// Which chats to release, and the `last_used` each decision was made on (the
/// sweep re-checks that stamp before it actually removes anything).
///
/// Three rules, in order: anything unpinned and idle for `idle_ms` goes; then
/// — if more than `max_open` chats would still be open — the least recently used
/// unpinned chats go until the cache is back under the bound; and then — if the
/// survivors' resident estimate still exceeds `byte_budget` (a few genuinely
/// huge docs, gh#414) — the least recently used unpinned chats keep going until
/// it does not. Pure, so the policy is testable without a clock, a socket or a
/// doc.
fn rooms_to_release(
    candidates: Vec<RoomCandidate>,
    now: i64,
    idle_ms: i64,
    max_open: usize,
    byte_budget: usize,
) -> Vec<(String, i64)> {
    let mut by_age = candidates;
    // Oldest first; chat id breaks ties so a sweep is deterministic.
    by_age.sort_by(|a, b| {
        a.last_used
            .cmp(&b.last_used)
            .then_with(|| a.chat_id.cmp(&b.chat_id))
    });
    let mut open = by_age.len();
    let mut resident: usize = by_age.iter().map(|c| c.resident).sum();
    let mut releasing = Vec::new();
    let mut taken = vec![false; by_age.len()];
    for (i, candidate) in by_age.iter().enumerate() {
        if candidate.pinned || now.saturating_sub(candidate.last_used) < idle_ms {
            continue;
        }
        taken[i] = true;
        releasing.push((candidate.chat_id.clone(), candidate.last_used));
        open -= 1;
        resident = resident.saturating_sub(candidate.resident);
    }
    // Over a bound: keep taking from the oldest end. A pinned chat is never
    // eligible, so a device with more than `max_open` LIVE chats simply stays
    // over the bound — cutting a live run's socket is not a trade worth making.
    for (i, candidate) in by_age.iter().enumerate() {
        if open <= max_open && resident <= byte_budget {
            break;
        }
        if candidate.pinned || taken[i] {
            continue;
        }
        taken[i] = true;
        releasing.push((candidate.chat_id.clone(), candidate.last_used));
        open -= 1;
        resident = resident.saturating_sub(candidate.resident);
    }
    releasing
}

/// The resumed-turn prompt for answers to a question whose run died: each
/// answer paired with its question text so the reattached conversation reads
/// naturally. Pure.
pub fn respond_input_prompt(
    questions: &[UserInputQuestion],
    answers: &[UserInputAnswer],
) -> String {
    let mut lines = vec!["Answering your earlier question:".to_string()];
    for answer in answers {
        let picked = answer.labels.join(", ");
        let question = questions
            .iter()
            .find(|q| q.id == answer.question_id)
            .map(|q| q.question.trim())
            .filter(|q| !q.is_empty());
        match question {
            Some(question) => lines.push(format!("{question} — {picked}")),
            None => lines.push(picked),
        }
    }
    lines.join("\n")
}

/// Per-chat background task: reacts to doc changes (local commits and remote imports)
/// by re-publishing the transcript watch, draining commands, and debouncing snapshots.
/// Holds only a weak handle so a dropped host tears the task down.
async fn chat_task(host: DocHost, weak: Weak<ChatDocHandle>, mut changed_rx: watch::Receiver<u64>) {
    // Initial pass: the snapshot may already carry pending commands. The
    // mirror stays lazy — it materializes on the first watch attach.
    {
        let Some(handle) = weak.upgrade() else { return };
        host.drain_commands(&handle).await;
    }
    let mut save_deadline: Option<tokio::time::Instant> = None;
    loop {
        let sleep_until = save_deadline.unwrap_or_else(tokio::time::Instant::now);
        tokio::select! {
            changed = changed_rx.changed() => {
                if changed.is_err() {
                    break; // doc handle (and its change sender) is gone
                }
                let Some(handle) = weak.upgrade() else { break };
                // A change is use: a local commit, or an import from the room.
                // Keeps a chat that is quietly syncing out of the idle sweep.
                handle.touch();
                handle.publish_messages_if_watched();
                host.drain_commands(&handle).await;
                if save_deadline.is_none() {
                    save_deadline = Some(
                        tokio::time::Instant::now()
                            + std::time::Duration::from_millis(SNAPSHOT_DEBOUNCE_MS),
                    );
                }
            }
            _ = tokio::time::sleep_until(sleep_until), if save_deadline.is_some() => {
                save_deadline = None;
                let Some(handle) = weak.upgrade() else { break };
                host.save_snapshot(&handle);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! gh#395 — the handle map lets go.
    //!
    //! The policy is tested twice over: once as the pure decision
    //! ([`rooms_to_release`]), and once through a real [`DocHost`] whose chats
    //! are opened, released and re-opened, because the part that matters to a
    //! user is that a released chat comes back with its transcript intact.
    //!
    //! Edge-less throughout (`edge: None`): what a release does to a live
    //! websocket cannot be asserted in a unit test, and as of this commit the
    //! edge is refusing every request on its free-tier duration cap, so it has
    //! not been asserted against a live one either.

    use super::*;

    /// No byte budget: the tests that predate it drive the first two rules.
    const NO_BUDGET: usize = usize::MAX;

    fn candidate(chat_id: &str, last_used: i64, pinned: bool) -> RoomCandidate {
        sized_candidate(chat_id, last_used, pinned, 0)
    }

    fn sized_candidate(
        chat_id: &str,
        last_used: i64,
        pinned: bool,
        resident: usize,
    ) -> RoomCandidate {
        RoomCandidate {
            chat_id: chat_id.to_string(),
            last_used,
            pinned,
            resident,
        }
    }

    fn released_ids(released: Vec<(String, i64)>) -> Vec<String> {
        let mut ids: Vec<String> = released.into_iter().map(|(id, _)| id).collect();
        ids.sort();
        ids
    }

    // ── the policy ──────────────────────────────────────────────────────────

    #[test]
    fn releases_only_chats_idle_past_the_window() {
        let now = 1_000_000;
        let released = rooms_to_release(
            vec![
                candidate("idle", now - 60_000, false),
                candidate("just-inside", now - 29_000, false),
                candidate("fresh", now - 10, false),
            ],
            now,
            30_000,
            100,
            NO_BUDGET,
        );
        assert_eq!(released_ids(released), vec!["idle".to_string()]);
    }

    #[test]
    fn a_pinned_chat_is_never_released_however_old() {
        // A watcher, a live run, a caller mid-write — the sweep cannot tell
        // which, and does not need to: all three mean "in use".
        let now = 1_000_000;
        let released = rooms_to_release(
            vec![
                candidate("watched", 0, true),
                candidate("running", 0, true),
                candidate("nobody", 0, false),
            ],
            now,
            30_000,
            100,
            NO_BUDGET,
        );
        assert_eq!(released_ids(released), vec!["nobody".to_string()]);
    }

    #[test]
    fn the_bound_releases_the_least_recently_used_even_when_fresh() {
        // Every chat is inside the idle window, so only the cap can act.
        let now = 1_000_000;
        let candidates: Vec<RoomCandidate> = (0..6)
            .map(|i| candidate(&format!("chat-{i}"), now - (6 - i) * 1_000, false))
            .collect();
        let released = rooms_to_release(candidates, now, 30_000, 4, NO_BUDGET);
        assert_eq!(
            released_ids(released),
            vec!["chat-0".to_string(), "chat-1".to_string()],
            "the two oldest go; the cache lands exactly on the bound"
        );
    }

    #[test]
    fn the_bound_never_takes_a_pinned_chat() {
        // Six live chats and a bound of four: the bound loses. Cutting a room
        // out from under a live run would cost the run its sync.
        let now = 1_000_000;
        let mut candidates: Vec<RoomCandidate> = (0..6)
            .map(|i| candidate(&format!("live-{i}"), now - (6 - i) * 1_000, true))
            .collect();
        candidates.push(candidate("spare", now - 500, false));
        let released = rooms_to_release(candidates, now, 30_000, 4, NO_BUDGET);
        assert_eq!(
            released_ids(released),
            vec!["spare".to_string()],
            "only the unpinned one is eligible, and the cache stays over the bound"
        );
    }

    #[test]
    fn the_idle_sweep_counts_toward_the_bound() {
        // Three idle + three fresh, bound 4: the idle three already put the
        // cache under it, so no fresh chat is taken as well.
        let now = 1_000_000;
        let mut candidates: Vec<RoomCandidate> = (0..3)
            .map(|i| candidate(&format!("idle-{i}"), now - 90_000, false))
            .collect();
        candidates.extend((0..3).map(|i| candidate(&format!("fresh-{i}"), now - 100, false)));
        let released = rooms_to_release(candidates, now, 30_000, 4, NO_BUDGET);
        assert_eq!(
            released_ids(released),
            vec![
                "idle-0".to_string(),
                "idle-1".to_string(),
                "idle-2".to_string()
            ]
        );
    }

    #[test]
    fn the_byte_budget_releases_oldest_first_even_under_the_count_bound() {
        // Three fresh chats, all inside the idle window and under the count
        // bound — only the byte budget can act. Two 40MB docs and one 1MB one
        // (81MB resident) against a 40MB budget: releasing the oldest alone
        // leaves 41MB, still over, so the sweep keeps going from the oldest
        // end and takes `small` too — 40MB fits and it stops there, leaving
        // the newest huge doc resident (gh#414 — a handful of genuinely huge
        // docs). The budget has to sit under 41MB for the second release to
        // be necessary at all; at 60MB the first release already fits and
        // taking `small` would be gratuitous.
        let now = 1_000_000;
        let mb = 1024 * 1024;
        let released = rooms_to_release(
            vec![
                sized_candidate("huge-old", now - 3_000, false, 40 * mb),
                sized_candidate("huge-new", now - 1_000, false, 40 * mb),
                sized_candidate("small", now - 2_000, false, mb),
            ],
            now,
            30_000,
            100,
            40 * mb,
        );
        assert_eq!(
            released_ids(released),
            vec!["huge-old".to_string(), "small".to_string()],
            "oldest unpinned go until the resident estimate fits the budget"
        );
    }

    #[test]
    fn the_byte_budget_never_takes_a_pinned_chat() {
        let now = 1_000_000;
        let mb = 1024 * 1024;
        let released = rooms_to_release(
            vec![
                sized_candidate("huge-live", now - 3_000, true, 100 * mb),
                sized_candidate("small", now - 1_000, false, mb),
            ],
            now,
            30_000,
            100,
            50 * mb,
        );
        assert_eq!(
            released_ids(released),
            vec!["small".to_string()],
            "a watched/running doc stays resident even over the budget"
        );
    }

    #[test]
    fn nothing_to_do_is_nothing_released() {
        let now = 1_000_000;
        assert!(rooms_to_release(Vec::new(), now, 30_000, 4, NO_BUDGET).is_empty());
        assert!(
            rooms_to_release(vec![candidate("fresh", now, false)], now, 30_000, 4, NO_BUDGET).is_empty(),
            "a quiet, under-bound cache is left alone"
        );
    }

    // ── the cache ───────────────────────────────────────────────────────────

    fn host(dir: &std::path::Path) -> DocHost {
        let store = Arc::new(DocsStore::open(dir).expect("docs store"));
        DocHost::new(
            store,
            DocHostConfig {
                device_id: "device-a".into(),
                default_harness: HarnessId::Mock,
                edge: None,
            },
        )
    }

    /// Let the per-chat tasks run their initial pass: each one holds an
    /// upgraded handle while it publishes and drains, which is (correctly) a
    /// pin. Nothing about the policy depends on this — it is the test getting
    /// out of the way of the code it is testing.
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    #[tokio::test]
    async fn an_unused_chat_is_released_and_comes_back_whole() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = host(dir.path());

        let handle = host.open("chat-a").expect("open");
        handle
            .write_user_message("m1", "the thing I said", 1)
            .expect("write");
        drop(handle); // nobody is holding it now
        settle().await;
        assert_eq!(host.open_chats(), vec!["chat-a".to_string()]);

        assert_eq!(host.release_idle_at(now_ms(), 0, 100, NO_BUDGET), 1);
        assert!(
            host.open_chats().is_empty(),
            "the cache entry ended, so the room census stops counting it"
        );

        // Transparent re-open: same chat, same transcript, from the snapshot
        // the release wrote — a different handle underneath, which is precisely
        // what no caller can tell.
        let reopened = host.open("chat-a").expect("re-open");
        let entries = reopened.doc().read_entries().expect("entries");
        assert_eq!(entries.len(), 1, "transcript survived the release");
        assert_eq!(entries[0].id, "m1");
        assert_eq!(host.open_chats(), vec!["chat-a".to_string()]);
    }

    #[tokio::test]
    async fn the_lazy_mirror_materializes_on_attach() {
        // gh#414: unwatched docs no longer maintain the transcript mirror per
        // commit — the first watch attach must still see the whole transcript.
        let dir = tempfile::tempdir().expect("tempdir");
        let host = host(dir.path());

        let handle = host.open("chat-lazy").expect("open");
        handle
            .write_user_message("m1", "written while unwatched", 1)
            .expect("write");
        settle().await;
        // Nobody watched: the mirror is deliberately empty…
        assert!(handle.messages_tx.borrow().is_empty());
        // …and attaching rebuilds it before the first borrow.
        let rx = handle.watch_messages();
        let seen = rx.borrow().clone();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].id, "m1");
    }

    #[tokio::test]
    async fn a_purged_chat_loses_handle_and_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = host(dir.path());

        let handle = host.open("chat-doomed").expect("open");
        handle.write_user_message("m1", "gone soon", 1).expect("write");
        host.flush_all(); // persist so the purge has a snapshot to delete
        drop(handle);
        settle().await;

        host.purge_chat("chat-doomed");
        assert!(host.open_chats().is_empty(), "handle dropped");
        // A re-open starts from nothing: the snapshot is gone too.
        let reopened = host.open("chat-doomed").expect("re-open");
        assert!(reopened.doc().read_entries().expect("entries").is_empty());
    }

    #[tokio::test]
    async fn a_watched_chat_is_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = host(dir.path());

        let watcher = {
            let handle = host.open("chat-watched").expect("open");
            handle.watch_messages() // the handle goes; the watcher stays
        };
        host.open("chat-quiet").expect("open");
        settle().await;

        assert_eq!(host.release_idle_at(now_ms(), 0, 100, NO_BUDGET), 1);
        assert_eq!(
            host.open_chats(),
            vec!["chat-watched".to_string()],
            "releasing a watched chat would end its transcript stream"
        );

        // …and once the viewer leaves, it goes like any other.
        drop(watcher);
        assert_eq!(host.release_idle_at(now_ms(), 0, 100, NO_BUDGET), 1);
        assert!(host.open_chats().is_empty());
    }

    #[tokio::test]
    async fn a_chat_someone_is_holding_is_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = host(dir.path());

        let held = host.open("chat-held").expect("open");
        host.open("chat-loose").expect("open");
        settle().await;

        assert_eq!(host.release_idle_at(now_ms(), 0, 100, NO_BUDGET), 1);
        assert_eq!(host.open_chats(), vec!["chat-held".to_string()]);
        drop(held);
        assert_eq!(host.release_idle_at(now_ms(), 0, 100, NO_BUDGET), 1);
        assert!(host.open_chats().is_empty());
    }

    #[tokio::test]
    async fn idleness_is_measured_from_the_last_use_not_the_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = host(dir.path());
        let first = host.open("chat-a").expect("open").last_used();
        tokio::time::sleep(Duration::from_millis(5)).await;
        let latest = host.open("chat-a").expect("re-open").last_used();
        settle().await;
        assert!(latest > first, "the second open moved the clock");

        let window = latest - first;
        assert_eq!(
            host.release_idle_at(first + window, window, 100, NO_BUDGET),
            0,
            "idle by the FIRST open's clock, but it has been used since"
        );
        assert_eq!(host.open_chats(), vec!["chat-a".to_string()]);
        assert_eq!(
            host.release_idle_at(latest + window, window, 100, NO_BUDGET),
            1,
            "the window has now passed since the last use"
        );
        assert!(host.open_chats().is_empty());
    }

    #[tokio::test]
    async fn the_bound_trims_the_oldest_open_chats() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = host(dir.path());
        for i in 0..5 {
            host.open(&format!("chat-{i}")).expect("open");
            // Distinct `last_used` stamps, so "least recently used" is a fact
            // rather than a tie broken by the chat id.
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        settle().await;

        // Nothing is idle (window of an hour), so only the bound can act.
        assert_eq!(host.release_idle_at(now_ms(), 3_600_000, 3, NO_BUDGET), 2);
        assert_eq!(
            host.open_chats(),
            vec![
                "chat-2".to_string(),
                "chat-3".to_string(),
                "chat-4".to_string()
            ],
            "the two least recently opened were released"
        );
    }
}
