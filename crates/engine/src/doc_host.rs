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
//!   ledger), execute recoverable Run/Queue commands, durably snapshot their terminal
//!   outcome, and only then mark them processed so neither crash window can strand or
//!   duplicate a paid turn; other side effects retain mark-before-execute.
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
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, RwLock, Weak};
use std::time::Duration;

use tokio::sync::watch;

use comet_doc::{
    COMMAND_DEFAULT_TTL_MS, CommandBasedOn, CommandDisposition, DocError, EvaluationContext,
    MessagePart, MessageRole, MessageStatus, QueueView, SessionCommandEntry, SessionCommandPayload,
    SessionCommandStatus, SessionDoc, SessionMessageEntry, evaluate_command,
    join_continuation_entries, queue,
};
use comet_proto::{ContextRef, HarnessId, UserInputAnswer, UserInputQuestion};
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

/// How often an open chat asks whether a recovery is waiting on independent
/// verification (gh#483). A keyed lookup against a table that is empty on every
/// chat that has never recovered, which is nearly all of them.
const CONVERGENCE_VERIFY_POLL: Duration = Duration::from_secs(30);

/// How long the fresh verification client waits for the room to hand it every
/// stable id. Generous: it is one dial after a rare event, and giving up early
/// only means keeping the quarantine another 30 seconds.
const CONVERGENCE_VERIFY_BUDGET: Duration = Duration::from_secs(60);

/// Crash injection for the recoverable-Run protocol. It deliberately leaves
/// the durable command Pending and the in-memory start claim held, exactly as
/// abrupt process death would; a newly assembled host has no such claim and
/// must therefore run the command.
#[cfg(test)]
static FAIL_BEFORE_RUN_DISPATCH: OnceLock<Mutex<Option<String>>> = OnceLock::new();
#[cfg(test)]
static FAIL_AFTER_OUTCOME_PERSIST: OnceLock<Mutex<Option<String>>> = OnceLock::new();

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

/// Content-convergence counts across this engine's open chats (gh#483).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConvergenceCensus {
    /// Chats holding content the edge has not acknowledged — recovering and
    /// blocked chats included, because both are ways of not being converged.
    pub unconverged: usize,
    pub recovering: usize,
    pub blocked: usize,
    /// Chats whose unacknowledged content has been unacknowledged past
    /// `comet_sync::UNACKNOWLEDGED_ALERT` — on a live socket (gh#483 §7).
    ///
    /// Kept apart from [`Self::unconverged`] because the two grade differently
    /// (gh#527 review): a room that is merely pending is a write in flight,
    /// which is what a healthy fleet looks like most seconds of the day, while
    /// a room that has been pending for two minutes on a live socket is the
    /// gh#483 incident in progress. A check that failed on the first would cry
    /// wolf on every keystroke; one that cannot see the second prints `ok` over
    /// 262 entries that exist on one device.
    pub stalled: usize,
    pub unacknowledged_entries: usize,
    /// The chats behind those counts, named — worst first (blocked, then
    /// recovering, then merely pending), so a truncated view still shows the
    /// room an operator most needs to open.
    pub rooms: Vec<comet_proto::RoomConvergence>,
}

struct DocHostInner {
    store: Arc<DocsStore>,
    config: DocHostConfig,
    sessions: OnceLock<SessionsEngine>,
    workspace: OnceLock<WorkspaceHost>,
    repos: OnceLock<crate::Repos>,
    handles: Mutex<HashMap<String, Arc<ChatDocHandle>>>,
    /// Commands whose session snapshot must be durable before the executor may
    /// see them. Checkout-preparation handoff is the only producer.
    durable_pending: Mutex<HashSet<String>>,
    /// Run commands between durable queueing and a successful
    /// `SessionsEngine::dispatch`. They stay Pending in the document until the
    /// sessions engine owns a live run; this in-process claim prevents two
    /// drain tasks starting the same command while a crash simply clears it
    /// and makes the durable Pending entry recoverable.
    starting_runs: Mutex<HashSet<String>>,
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
    doc: RwLock<Arc<SessionDoc>>,
    changed_tx: watch::Sender<u64>,
    messages_tx: watch::Sender<Vec<SessionMessageEntry>>,
    /// The follow-up plan, folded from the command ledger (gh#424) and
    /// republished on every commit that anybody is watching for.
    queue_tx: watch::Sender<QueueView>,
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
    /// This chat's convergence driver (gh#483): the durable semantic outbox,
    /// the recovery quarantine, and the truthful content state. Shared with the
    /// room client, which drives the phases; the host records into it on every
    /// snapshot save and asks it what is still local-only.
    convergence: comet_sync::ConvergenceRecovery,
    /// Document version at the last journal record. Semantic content cannot
    /// change without the version changing, so an unchanged version means the
    /// outbox is already current and the save can skip the whole scan.
    journaled_version: Mutex<Option<Vec<u8>>>,
    /// Doc subscription (drop = unsubscribe) — bumps the change watch on every commit.
    _sub: Mutex<loro::Subscription>,
}

fn reconcile_device_commands(
    stale: &SessionDoc,
    replacement: &SessionDoc,
    device_id: &str,
) -> Result<usize, DocError> {
    let existing: HashMap<String, SessionCommandStatus> = replacement
        .read_commands()?
        .into_iter()
        .map(|command| (command.id, command.status))
        .collect();
    let mut replayed = 0;
    for command in stale
        .read_commands()?
        .into_iter()
        .filter(|command| command.issued_by == device_id)
    {
        match existing.get(&command.id) {
            None => {
                replacement.queue_command(&command)?;
                replayed += 1;
            }
            Some(SessionCommandStatus::Pending)
                if command.status != SessionCommandStatus::Pending =>
            {
                replacement.set_command_status(
                    &command.id,
                    command.status,
                    command.resolution.as_deref(),
                )?;
                replayed += 1;
            }
            _ => {}
        }
    }
    Ok(replayed)
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
        // The plan watch counts too: it is fed the same way and ends the same
        // way, so releasing a chat somebody has a queue tray open on would cut
        // that tray off exactly as it would cut a transcript off.
        self.messages_tx.receiver_count() > 0 || self.queue_tx.receiver_count() > 0
    }

    pub fn doc(&self) -> Arc<SessionDoc> {
        self.doc
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn doc_arc(&self) -> Arc<SessionDoc> {
        self.doc()
    }

    fn with_current<R>(&self, operation: impl FnOnce(&SessionDoc) -> R) -> R {
        let owner = self.doc.read().unwrap_or_else(PoisonError::into_inner);
        operation(owner.as_ref())
    }

    fn reseed(&self, raw: loro::LoroDoc) -> Result<usize, DocError> {
        let replacement = Arc::new(SessionDoc::from_doc(raw));
        // Device command creation holds a read guard for its entire commit.
        // Taking the write guard therefore closes the replay-to-swap gap: a
        // command either finishes on the old owner and is copied below, or
        // starts after the replacement is visible and writes there directly.
        let mut owner = self.doc.write().unwrap_or_else(PoisonError::into_inner);
        let replayed =
            reconcile_device_commands(owner.as_ref(), replacement.as_ref(), &self.device_id)?;
        let changed_tx = self.changed_tx.clone();
        let sub = replacement.doc().subscribe_root(Arc::new(move |_diff| {
            changed_tx.send_modify(|v| *v = v.wrapping_add(1));
        }));
        *owner = replacement;
        drop(owner);
        *lock(&self._sub) = sub;
        // The server snapshot and replay commits happened before this new
        // subscription existed. Publish the ownership swap itself so mirrors
        // and disk persistence immediately read the replacement.
        self.changed_tx.send_modify(|v| *v = v.wrapping_add(1));
        Ok(replayed)
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

    /// The follow-up plan watch — the projection, re-sent on every doc change
    /// (`WatchDocQueue`, gh#424).
    ///
    /// A watch of the *derived* plan rather than of the raw ledger: every
    /// viewport would otherwise fold the same log itself, and a device that
    /// folded a beat differently would show a different plan for the same doc.
    /// The fold is pure and cheap, so the host does it once.
    pub fn watch_queue(&self) -> watch::Receiver<QueueView> {
        self.touch();
        let rx = self.queue_tx.subscribe();
        self.publish_queue();
        rx
    }

    /// The plan as it stands, folded from this doc's ledger.
    pub fn queue_view(&self) -> QueueView {
        match self.with_current(|doc| doc.read_commands()) {
            Ok(commands) => queue::project(&commands),
            Err(err) => {
                tracing::warn!(chat = %self.chat_id, error = %err, "command read failed");
                QueueView::default()
            }
        }
    }

    fn publish_queue(&self) {
        self.queue_tx.send_replace(self.queue_view());
    }

    /// Per-commit publish path, watched-only for the same reason the transcript
    /// mirror is: folding a plan nobody is looking at is per-commit work on
    /// every open doc, and the fold is rebuilt on attach anyway.
    fn publish_queue_if_watched(&self) {
        if self.queue_tx.receiver_count() > 0 {
            self.publish_queue();
        }
    }

    /// Record everything this document holds into the durable semantic outbox
    /// (gh#483 §2), on the same beat as the snapshot save.
    ///
    /// The outbox is what puts locally committed transcript content back after
    /// a shallow-history replacement, so it has to be written by the thing that
    /// makes local content durable — not by the room, which may not exist (an
    /// offline device commits plenty) and may be replaced mid-recovery.
    ///
    /// Cheap when nothing changed: semantic content cannot move without the
    /// document's version moving, so an unchanged version skips the scan
    /// entirely — the common case for an open-but-idle chat.
    fn journal_semantics(&self) {
        let doc = self.doc();
        self.journal_semantics_of(&doc);
    }

    /// [`Self::journal_semantics`] against a document the caller already holds.
    ///
    /// The distinction is not cosmetic: `persist_command_outcome_with_hook`
    /// runs inside `with_current`, i.e. holding the owner read guard, and a
    /// second acquisition from in there deadlocks against a reseed's pending
    /// write guard — which is exactly the boundary those tests drive.
    fn journal_semantics_of(&self, doc: &SessionDoc) {
        let version = doc.doc().oplog_vv().encode();
        if lock(&self.journaled_version).as_deref() == Some(version.as_slice()) {
            return;
        }
        match self.convergence.record(doc.doc()) {
            Ok(_) => *lock(&self.journaled_version) = Some(version),
            Err(err) => tracing::warn!(
                chat = %self.chat_id, error = %err,
                "could not record local content into the convergence outbox"
            ),
        }
    }

    /// What this chat can honestly say about its CONTENT (gh#483 §4).
    ///
    /// Read from the room when there is one — it holds the acknowledgement
    /// accounting — and from the outbox when there is not, because a chat with
    /// no room still knows perfectly well that its content is local-only. The
    /// one answer this never gives is "converged because the socket is up".
    pub fn convergence_state(&self) -> comet_sync::ConvergenceState {
        if let Some(state) = lock(&self.room).as_ref().map(|room| room.convergence()) {
            return state;
        }
        let unacked = self.convergence.pending();
        if unacked == 0 {
            comet_sync::ConvergenceState::Converged
        } else {
            comet_sync::ConvergenceState::Pending {
                unacked,
                stalled: false,
            }
        }
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
        let doc = self.doc();
        if doc.read_entries()?.iter().any(|e| e.id == message_id) {
            return Ok(());
        }
        doc.push_message(&SessionMessageEntry {
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

    /// Write a notice into the transcript: something the *engine* did, in the
    /// place a person is already looking.
    ///
    /// `System` rather than `User` because nobody typed it — the composers'
    /// echo logic and the sidebar preview both key on `User`, so a notice that
    /// borrowed that role would show up as a message the operator sent and
    /// would displace the last real one in the sidebar. `is_error` picks the
    /// part kind, which is what makes a failure read as one.
    ///
    /// Deduped by id like [`Self::write_user_message`], so a retried write —
    /// or a second engine holding the same doc — leaves one entry.
    pub fn write_notice(
        &self,
        message_id: &str,
        text: &str,
        is_error: bool,
        created_at: i64,
    ) -> Result<(), DocError> {
        let doc = self.doc();
        if doc.read_entries()?.iter().any(|e| e.id == message_id) {
            return Ok(());
        }
        let part = if is_error {
            MessagePart::Error {
                id: "e0".into(),
                message: text.to_string(),
            }
        } else {
            MessagePart::Text {
                id: "t0".into(),
                text: text.to_string(),
            }
        };
        doc.push_message(&SessionMessageEntry {
            id: message_id.to_string(),
            role: MessageRole::System,
            parts: vec![part],
            created_at,
            device_id: self.device_id.clone(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        })
    }

    /// Write a complete system message, idempotent by id, and publish it.
    pub fn write_system_message(
        &self,
        message_id: &str,
        text: &str,
        created_at: i64,
    ) -> Result<(), DocError> {
        self.write_notice(message_id, text, false, created_at)?;
        self.publish_messages();
        Ok(())
    }

    /// Recovery sweep: stamp this device's abandoned `streaming` entries `aborted`, appending
    /// `note` as a visible error part so the transcript says WHY the turn
    /// ended (comet folded "Run interrupted by backend restart" the same
    /// way). Returns the stamped entries' `(id, created_at)` — recovery uses
    /// them for the resume-freshness check.
    pub fn mark_abandoned_streams(&self, note: &str) -> Result<Vec<(String, i64)>, DocError> {
        let mut stamped = Vec::new();
        let doc = self.doc();
        for entry in doc.read_entries()? {
            if entry.role == MessageRole::Assistant
                && entry.status == Some(MessageStatus::Streaming)
                && entry.device_id == self.device_id
                && doc.set_message_status(&entry.id, MessageStatus::Aborted)?
            {
                let part_id = format!("{}-recovery", entry.id);
                if let Err(err) = doc.append_error_part(&entry.id, &part_id, note) {
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
        match self.doc().read_entries() {
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
                repos: OnceLock::new(),
                handles: Mutex::new(HashMap::new()),
                durable_pending: Mutex::new(HashSet::new()),
                starting_runs: Mutex::new(HashSet::new()),
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

    pub fn set_repos(&self, repos: crate::Repos) {
        let _ = self.inner.repos.set(repos);
    }

    /// The workspace host, once wired (tests may assemble a DocHost without one).
    pub fn workspace(&self) -> Option<&WorkspaceHost> {
        self.inner.workspace.get()
    }

    /// The harness a run in a chat with no `config` row uses — what a fork of
    /// such a chat inherits when its caller names none either (gh#425).
    pub fn default_harness(&self) -> comet_proto::HarnessId {
        self.inner.config.default_harness
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

    /// The CONTENT half of [`comet_proto::EdgeHealth`] (gh#483): how many open
    /// chats are not converged, recovering, or outright blocked, and how many
    /// semantic entries exist only on this device.
    ///
    /// Kept separate from [`Self::room_census`] because they answer different
    /// questions and the incident was the gap between them — a room can be in
    /// `chat_rooms_live` and in `unconverged` at the same time, and that pair
    /// is exactly what nobody could see before.
    pub fn convergence_census(&self) -> ConvergenceCensus {
        let mut census = ConvergenceCensus::default();
        if !self.edge_enabled() {
            return census;
        }
        let handles: Vec<_> = lock(&self.inner.handles).values().cloned().collect();
        // (severity, row) — the sort key is dropped before the census leaves.
        let mut ranked: Vec<(u8, comet_proto::RoomConvergence)> = Vec::new();
        for handle in handles {
            let state = handle.convergence_state();
            census.unacknowledged_entries += state.unacked();
            let severity = match state {
                comet_sync::ConvergenceState::Converged => continue,
                comet_sync::ConvergenceState::Pending { stalled, .. } => {
                    census.unconverged += 1;
                    if stalled {
                        census.stalled += 1;
                    }
                    // Severity 0 in flight, 2 stalled: a stalled room outranks
                    // one that is merely RECOVERING, because recovery is the
                    // machinery working and a stall is the machinery not.
                    if stalled { 2 } else { 0 }
                }
                comet_sync::ConvergenceState::Recovering { .. } => {
                    census.recovering += 1;
                    census.unconverged += 1;
                    1
                }
                comet_sync::ConvergenceState::BlockedLocalOnly { .. } => {
                    census.blocked += 1;
                    census.unconverged += 1;
                    3
                }
            };
            ranked.push((
                severity,
                comet_proto::RoomConvergence {
                    chat_id: handle.chat_id.clone(),
                    state: state.label(),
                    unacknowledged_entries: state.unacked(),
                },
            ));
        }
        // Worst first, then by how much content is at stake: a truncated view
        // still shows the room an operator most needs to open.
        ranked.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then(b.1.unacknowledged_entries.cmp(&a.1.unacknowledged_entries))
                .then(a.1.chat_id.cmp(&b.1.chat_id))
        });
        census.rooms = ranked.into_iter().map(|(_, room)| room).collect();
        census
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
        let root_changed_tx = changed_tx.clone();
        let sub = doc.doc().subscribe_root(Arc::new(move |_diff| {
            root_changed_tx.send_modify(|v| *v = v.wrapping_add(1));
        }));
        // The mirror starts dirty and empty: many opens (command queueing,
        // drains, nudges) never watch the transcript, and the first
        // watch_messages attach materializes it on demand.
        let (messages_tx, _) = watch::channel(Vec::new());
        let (queue_tx, _) = watch::channel(QueueView::default());

        let handle = Arc::new(ChatDocHandle {
            chat_id: chat_id.to_string(),
            device_id: self.inner.config.device_id.clone(),
            doc: RwLock::new(doc.clone()),
            changed_tx: changed_tx.clone(),
            messages_tx,
            queue_tx,
            room: Arc::new(Mutex::new(None)),
            last_used: AtomicI64::new(now_ms()),
            mirror_dirty: AtomicBool::new(true),
            snapshot_bytes: AtomicUsize::new(snapshot_len),
            convergence: comet_sync::ConvergenceRecovery::new(self.inner.store.clone(), chat_id),
            journaled_version: Mutex::new(None),
            _sub: Mutex::new(sub),
        });
        // A recovery interrupted by a crash finishes before anything else sees
        // this document (gh#483 §3/§6). Idempotent, and a no-op for the
        // overwhelming majority of opens that have no open recovery.
        match handle.convergence.resume(doc.doc()) {
            Ok(Some(report)) if !report.is_empty() => tracing::warn!(
                chat = %chat_id,
                replayed = report.total(),
                "finished an interrupted convergence recovery while opening the chat"
            ),
            Ok(_) => {}
            Err(err) => tracing::error!(
                chat = %chat_id, error = %err,
                "could not finish an interrupted convergence recovery; quarantine retained"
            ),
        }
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
            let weak_handle = Arc::downgrade(&handle);
            crate::workspace_host::spawn_room_join(
                edge.room_url_as(
                    format!("/session/{chat_id}/ws"),
                    Some(&self.inner.config.device_id),
                ),
                chat_id.to_string(),
                doc.doc().clone(),
                Some(self.inner.config.device_id.clone()),
                Arc::new(move |replacement| {
                    if let Some(handle) = weak_handle.upgrade()
                        && let Err(err) = handle.reseed(replacement)
                    {
                        tracing::error!(chat = %handle.chat_id, error = %err,
                            "failed to reconcile device commands during room reseed");
                    }
                }),
                None,
                // The durable outbox and quarantine (gh#483). Without this the
                // room falls back to a process-local journal, which cannot keep
                // a promise about surviving a restart — and this is the one
                // place a restart is the failure mode.
                Some((
                    self.inner.store.clone() as Arc<dyn comet_sync::ConvergenceJournal>,
                    chat_id.to_string(),
                )),
                Arc::downgrade(&handle.room),
                Arc::new(|| {}),
            );
            self.spawn_convergence_verification(&handle);
        }

        tokio::spawn(chat_task(self.clone(), Arc::downgrade(&handle), changed_rx));
        Ok(handle)
    }

    /// Watch for a recovery that has reached
    /// [`comet_sync::RecoveryPhase::Acknowledged`] and prove convergence with a
    /// FRESH client before releasing its quarantine (gh#483 §3).
    ///
    /// The edge acknowledging our push is its own word for it. The invariant
    /// asks for someone else's: an independent client, starting from an empty
    /// document, that can retrieve every stable id the quarantine holds. Only
    /// then are the quarantined bytes redundant.
    ///
    /// Costs one extra dial per recovery, which is a rare event; the poll
    /// itself is a keyed lookup against an all-but-always-empty table, and the
    /// task ends with the handle.
    fn spawn_convergence_verification(&self, handle: &Arc<ChatDocHandle>) {
        let Some(edge) = self.inner.config.edge.clone() else {
            return;
        };
        let chat_id = handle.chat_id.clone();
        let convergence = handle.convergence.clone();
        let weak = Arc::downgrade(handle);
        let device_id = self.inner.config.device_id.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(CONVERGENCE_VERIFY_POLL).await;
                if weak.upgrade().is_none() {
                    return; // chat released — nothing left to verify for
                }
                let Some(record) = convergence.open_recovery() else {
                    continue;
                };
                if record.phase != comet_sync::RecoveryPhase::Acknowledged {
                    continue;
                }
                let url =
                    edge.room_url_as(format!("/session/{chat_id}/ws"), Some(device_id.as_str()));
                let observed = match RoomClient::observe_stable_ids(
                    url,
                    &chat_id,
                    &record.expected_ids,
                    CONVERGENCE_VERIFY_BUDGET,
                )
                .await
                {
                    Ok(observed) => observed,
                    Err(err) => {
                        tracing::warn!(chat = %chat_id, error = %err,
                            "independent convergence check could not reach the room; retrying");
                        continue;
                    }
                };
                match convergence.confirm_independent(&observed) {
                    Ok(Some(missing)) if missing.is_empty() => {
                        tracing::info!(chat = %chat_id,
                            "shallow-history recovery verified end to end; quarantine released");
                        return;
                    }
                    Ok(Some(missing)) => tracing::error!(
                        chat = %chat_id,
                        missing = missing.len(),
                        "a fresh client cannot retrieve everything this device recovered; \
                         the quarantine stays until it can"
                    ),
                    Ok(None) => return, // released by somebody else
                    Err(err) => tracing::warn!(chat = %chat_id, error = %err,
                        "convergence verification bookkeeping failed"),
                }
            }
        });
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

    /// §7 nudge entry point: open (or touch) the chat doc, close any orphaned
    /// streaming turn, kick the drain, and report the chat's drain-relevant
    /// state for the log — "nudge: chat doc opened" followed by silence was
    /// undiagnosable (gh#528).
    pub fn nudge(&self, chat_id: &str) -> Result<String, EngineError> {
        let handle = self.open(chat_id)?;
        self.reconcile_orphaned_streams(&handle);
        // A cached handle got no fresh initial drain: re-kick it, so a command
        // whose import raced an earlier drain still executes on the nudge.
        handle
            .changed_tx
            .send_modify(|value| *value = value.wrapping_add(1));
        Ok(self.chat_state_summary(&handle))
    }

    /// Close a turn a dead run left streaming (gh#528).
    ///
    /// The journal-driven boot sweep (`SessionsEngine::recover_stale`) only
    /// sees chats whose journal is still open; a journal closed by a LATER
    /// completed turn — or missing entirely — leaves the doc's `streaming`
    /// entry orphaned forever: the transcript renders a live turn nothing will
    /// finish, and the fork gate refuses its tail. Reconcile at the doc: a
    /// streaming entry of ours, on a chat we host, with no live run and no
    /// still-open journal, is an orphan — stamp it `aborted`, loudly.
    ///
    /// Runs on every fresh open (boot warm-open, nudge, watcher) and on every
    /// nudge of a cached handle. Safe against a live turn twice over: a run
    /// holds `chat_is_busy` for its whole life, and the run task finalizes its
    /// segment before deregistering. The journal guard keeps boot ordering
    /// honest: a still-open journal is `recover_stale`'s case, and its
    /// auto-resume freshness check reads the same entry this would stamp.
    pub(crate) fn reconcile_orphaned_streams(&self, handle: &Arc<ChatDocHandle>) {
        let Some(sessions) = self.inner.sessions.get() else {
            return;
        };
        if !self.is_host(handle) || sessions.chat_is_busy(&handle.chat_id) {
            return;
        }
        let has_orphan = handle.doc().read_entries().ok().is_some_and(|entries| {
            entries.iter().any(|e| {
                e.role == MessageRole::Assistant
                    && e.status == Some(MessageStatus::Streaming)
                    && e.device_id == handle.device_id
            })
        });
        if !has_orphan || sessions.journal_is_stale(&handle.chat_id) {
            return;
        }
        match handle.mark_abandoned_streams("Turn orphaned by an engine restart") {
            Ok(stamped) if !stamped.is_empty() => {
                sessions.note_orphan_cleared(&handle.chat_id);
                tracing::warn!(chat = %handle.chat_id, stamped = stamped.len(),
                    "closed orphaned streaming turn (no run owns it)");
            }
            Ok(_) => {}
            Err(err) => tracing::warn!(chat = %handle.chat_id, error = %err,
                "orphaned-stream reconcile failed"),
        }
    }

    /// One line of drain-relevant state: everything `drain_commands` silently
    /// declines on, plus what is pending and what the transcript tail says.
    fn chat_state_summary(&self, handle: &Arc<ChatDocHandle>) -> String {
        let chat_id = &handle.chat_id;
        let host = self.is_host(handle);
        let staged = self
            .inner
            .sessions
            .get()
            .is_some_and(|s| s.fork_is_staged(chat_id));
        let running = self.chat_is_running(chat_id);
        let pending = match handle.doc().read_commands() {
            Ok(commands) => commands
                .iter()
                .filter(|c| {
                    c.status == SessionCommandStatus::Pending
                        && !self.inner.store.is_processed(&c.id).unwrap_or(false)
                })
                .count()
                .to_string(),
            Err(_) => "?".into(),
        };
        let tail = handle
            .doc()
            .read_entries()
            .ok()
            .and_then(|entries| {
                entries.last().map(|e| {
                    let status = e
                        .status
                        .map(|s| format!("{s:?}"))
                        .unwrap_or_else(|| "none".into());
                    format!("{:?}:{status}", e.role)
                })
            })
            .unwrap_or_else(|| "empty".into());
        format!("host={host} fork_staged={staged} running={running} pending={pending} tail={tail}")
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
    fn release_idle_at(
        &self,
        now: i64,
        idle_ms: i64,
        max_open: usize,
        byte_budget: usize,
    ) -> usize {
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
        if let Err(err) = self.purge_chat_checked(chat_id) {
            tracing::warn!(chat = %chat_id, error = %err, "snapshot delete failed");
        }
    }

    pub fn purge_chat_checked(&self, chat_id: &str) -> Result<(), EngineError> {
        let removed = lock(&self.inner.handles).remove(chat_id);
        drop(removed);
        self.inner.store.delete_snapshot(chat_id)?;
        Ok(())
    }

    pub fn persist_chat_snapshot(&self, chat_id: &str) -> Result<(), EngineError> {
        let handle = self.open(chat_id)?;
        self.persist_snapshot(&handle)?;
        Ok(())
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
        self.queue_command_with_context(chat_id, payload, Vec::new())
    }

    /// [`Self::queue_command`] carrying typed `@` references (gh#424).
    ///
    /// The references ride the entry rather than the prompt text because the
    /// text is a text: it can be edited, pasted, re-wrapped, and a renderer's
    /// chip decoration cannot survive any of that, let alone a surface that
    /// draws no chips. What the host resolves at execute time is this list.
    pub fn queue_command_with_context(
        &self,
        chat_id: &str,
        payload: SessionCommandPayload,
        context: Vec<ContextRef>,
    ) -> Result<String, EngineError> {
        let id = new_id();
        self.queue_command_with_id_and_context(chat_id, &id, payload, context)?;
        Ok(id)
    }

    /// Queue a command under a caller-owned id. Used by checkout preparation:
    /// the id is parked durably before the chat exists, so retrying a release
    /// after a crash cannot append a second billable run.
    pub fn queue_command_with_id(
        &self,
        chat_id: &str,
        id: &str,
        payload: SessionCommandPayload,
    ) -> Result<(), EngineError> {
        self.queue_command_with_id_and_context(chat_id, id, payload, Vec::new())
    }

    pub fn queue_command_with_id_and_context(
        &self,
        chat_id: &str,
        id: &str,
        payload: SessionCommandPayload,
        context: Vec<ContextRef>,
    ) -> Result<(), EngineError> {
        let handle = self.open(chat_id)?;
        let now = now_ms();
        handle.with_current(|doc| {
            if let Some(existing) = doc
                .read_commands()?
                .into_iter()
                .find(|command| command.id == id)
            {
                if existing.payload == payload && existing.context == context {
                    return Ok(());
                }
                return Err(comet_doc::DocError::Schema(format!(
                    "command id collision: {id} already names different intent"
                )));
            }
            let based_on = doc.read_entries()?.last().map(|m| CommandBasedOn {
                turn_id: Some(m.id.clone()),
                frontier: None,
            });
            // A queued follow-up (and any mutation of the plan) carries no
            // TTL: it was deliberately written to wait.
            let expires_at = entry_expires(&payload).then_some(now + COMMAND_DEFAULT_TTL_MS);
            doc.queue_command(&SessionCommandEntry {
                id: id.to_string(),
                payload,
                issued_by: self.inner.config.device_id.clone(),
                issued_at: now,
                based_on,
                expires_at,
                status: SessionCommandStatus::Pending,
                resolution: None,
                context,
            })
        })?;
        // §7 durable delivery: when another device hosts this chat, nudge its device
        // room so a cold host opens the doc and drains the queue. Fire-and-forget —
        // the command is durable in the doc either way (a host that opens the chat
        // for any other reason still executes it).
        self.nudge_remote_host(chat_id);
        Ok(())
    }

    /// Queue under a stable id and force the containing session snapshot to
    /// disk before returning. Checkout preparation keeps its parked command
    /// until this acknowledgement: a crash between the in-memory append and
    /// the normal debounce can therefore replay the same id, never lose the
    /// only copy and never append a second run.
    pub fn queue_command_with_id_durable(
        &self,
        chat_id: &str,
        id: &str,
        payload: SessionCommandPayload,
    ) -> Result<(), EngineError> {
        self.queue_command_with_id_and_context_durable(chat_id, id, payload, Vec::new())
    }

    pub fn queue_command_with_id_and_context_durable(
        &self,
        chat_id: &str,
        id: &str,
        payload: SessionCommandPayload,
        context: Vec<ContextRef>,
    ) -> Result<(), EngineError> {
        lock(&self.inner.durable_pending).insert(id.to_string());
        if let Err(error) = self.queue_command_with_id_and_context(chat_id, id, payload, context) {
            lock(&self.inner.durable_pending).remove(id);
            return Err(error);
        }
        let handle = self.open(chat_id)?;
        // A failed save deliberately leaves the id gated. The caller keeps its
        // parked payload, and a retry persists this same command id before
        // opening the gate. No run starts from an unacknowledged memory append.
        self.persist_snapshot(&handle)?;
        lock(&self.inner.durable_pending).remove(id);
        // The notification raised by the append may already have drained while
        // this id was gated. Raise another after the durable acknowledgement.
        handle
            .changed_tx
            .send_modify(|value| *value = value.wrapping_add(1));
        Ok(())
    }

    /// The device hosting `chat_id`, when anything says: the session doc's own
    /// stamp first (it travels with a chat shared into the org — the only
    /// source a teammate has), then this user's workspace row. `None` = nobody
    /// has claimed it.
    fn host_device(&self, chat_id: &str) -> Option<String> {
        let stamped = lock(&self.inner.handles)
            .get(chat_id)
            .and_then(|handle| handle.doc().host_device_id());
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
        if let Some(host) = handle.doc().host_device_id() {
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
        if let Err(err) = handle
            .doc()
            .set_host_device_id(&self.inner.config.device_id)
        {
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

    /// Drain pending commands (host-only). `Run` is claimed in memory, handed
    /// to Sessions, and only then marked processed; non-Run side effects retain
    /// the command plane's mark-before-execute ordering.
    pub async fn drain_commands(&self, handle: &Arc<ChatDocHandle>) {
        let Some(sessions) = self.inner.sessions.get() else {
            return; // executor not wired yet; the set_sessions kick re-drains
        };
        // Fork commands may already be durable while the workspace row is
        // being published. Leave them Pending until the creation intent is
        // committed; rejecting a staged Run would consume the first prompt.
        if sessions.fork_is_staged(&handle.chat_id) {
            tracing::debug!(chat = %handle.chat_id, "drain: fork staged; commands stay pending");
            return;
        }
        if !self.is_host(handle) {
            tracing::debug!(chat = %handle.chat_id, "drain: not this chat's host");
            return;
        }
        // Entries this pass decided to leave alone (processed dedupe hits).
        let mut skipped: HashSet<String> = HashSet::new();
        loop {
            let doc = handle.doc();
            let commands = match doc.read_commands() {
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
                        && !lock(&self.inner.durable_pending).contains(&c.id)
                        && !lock(&self.inner.starting_runs).contains(&c.id)
                        && !is_processed(&c.id)
                })
                .cloned()
            else {
                return;
            };
            let messages = doc.read_entries().unwrap_or_default();
            let current_turn_id = messages.last().map(|m| m.id.clone());
            let turn_is_past = |turn_id: &str| messages.iter().any(|m| m.id == turn_id);
            let plan = queue::project(&commands);
            let disposition = evaluate_command(
                &entry,
                &EvaluationContext {
                    is_processed: &is_processed,
                    now_ms: now_ms(),
                    entries: &commands,
                    current_turn_id: current_turn_id.as_deref(),
                    turn_is_past: &turn_is_past,
                    queue: &plan,
                    chat_is_running: self.chat_is_running(&handle.chat_id),
                },
            );
            // A deferred follow-up remains durable and pending. It must not
            // enter the processed ledger until its turn is actually owned.
            if disposition == CommandDisposition::Defer {
                skipped.insert(entry.id.clone());
                continue;
            }

            // A Run is different from every other command side effect. The
            // sessions engine installs its RunHandle before dispatch returns,
            // and engine death tears that run down. Marking it processed
            // *before* dispatch created a permanent processed -> spawn crash
            // hole for checkout recovery. Keep the durable command Pending,
            // claim it in-process, and mark only after Sessions owns the run.
            let recoverable_run = matches!(disposition, CommandDisposition::Execute)
                && matches!(
                    &entry.payload,
                    SessionCommandPayload::Run { .. } | SessionCommandPayload::Queue { .. }
                );
            if recoverable_run {
                if !lock(&self.inner.starting_runs).insert(entry.id.clone()) {
                    skipped.insert(entry.id.clone());
                    continue;
                }
                #[cfg(test)]
                {
                    let failpoint = FAIL_BEFORE_RUN_DISPATCH.get_or_init(|| Mutex::new(None));
                    let mut command = lock(failpoint);
                    if command.as_deref() == Some(entry.id.as_str()) {
                        command.take();
                        return;
                    }
                }
                let (status, resolution) = match self.execute(sessions, handle, &entry).await {
                    Ok(outcome) => outcome,
                    Err(err) => (SessionCommandStatus::Rejected, Some(err.to_string())),
                };
                if status == SessionCommandStatus::Rejected {
                    tracing::warn!(chat = %handle.chat_id, command = %entry.id,
                        resolution = resolution.as_deref().unwrap_or(""),
                        "run command rejected");
                }
                if let Err(err) =
                    self.persist_command_outcome(handle, &entry.id, status, resolution.as_deref())
                {
                    tracing::error!(chat = %handle.chat_id, command = %entry.id, error = %err,
                        "run outcome snapshot failed; retaining command claim until restart");
                    return;
                }
                #[cfg(test)]
                {
                    let failpoint = FAIL_AFTER_OUTCOME_PERSIST.get_or_init(|| Mutex::new(None));
                    let mut command = lock(failpoint);
                    if command.as_deref() == Some(entry.id.as_str()) {
                        command.take();
                        return;
                    }
                }
                if let Err(err) = self.inner.store.mark_processed(&entry.id) {
                    // Retain the in-process claim. Re-running after Sessions
                    // has accepted the prompt would duplicate a turn; restart
                    // is the recovery point, when that run is gone and the
                    // still-Pending command is safe to start again.
                    tracing::error!(chat = %handle.chat_id, command = %entry.id, error = %err,
                        "run-start ledger write failed; retaining command claim until restart");
                    return;
                }
                lock(&self.inner.starting_runs).remove(&entry.id);
                continue;
            }

            // Non-Run commands retain the command plane's at-most-once rule:
            // their arbitrary side effect may survive an engine crash.
            if let Err(err) = self.inner.store.mark_processed(&entry.id) {
                tracing::error!(chat = %handle.chat_id, error = %err, "processed-ledger write failed; halting drain");
                return;
            }
            match disposition {
                CommandDisposition::Defer => unreachable!("handled above"),
                CommandDisposition::Skip => {
                    skipped.insert(entry.id.clone());
                }
                CommandDisposition::Expired => {
                    self.resolve_command(handle, &entry.id, SessionCommandStatus::Expired, None);
                }
                CommandDisposition::Superseded => {
                    self.resolve_command(handle, &entry.id, SessionCommandStatus::Superseded, None);
                }
                CommandDisposition::Cancelled => {
                    self.resolve_command(
                        handle,
                        &entry.id,
                        SessionCommandStatus::Cancelled,
                        Some("removed from the queue"),
                    );
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

    /// Is a turn live on this chat? Distinct from [`Self::chat_is_busy`], which
    /// answers "may the handle be released" and stays true for a PARKED
    /// persistent session — a follow-up gated on that would never run, because a
    /// steerable harness parks for thirty minutes after every completed turn.
    /// What a follow-up waits for is the turn, not the process.
    fn chat_is_running(&self, chat_id: &str) -> bool {
        self.inner.sessions.get().is_some_and(|sessions| {
            matches!(
                sessions.session_status(chat_id).map(|s| s.status),
                Some(
                    comet_proto::SessionStatus::Working | comet_proto::SessionStatus::AwaitingInput
                )
            )
        })
    }

    /// Host-only outcome write (ledger rule 2).
    fn resolve_command(
        &self,
        handle: &ChatDocHandle,
        command_id: &str,
        status: SessionCommandStatus,
        resolution: Option<&str>,
    ) {
        // A command that resolves without a turn must say so somewhere a
        // journal reader can see — a swallowed send was undiagnosable (gh#528).
        match status {
            SessionCommandStatus::Rejected => {
                tracing::warn!(chat = %handle.chat_id, command = %command_id,
                    resolution = resolution.unwrap_or(""), "command rejected");
            }
            SessionCommandStatus::Applied | SessionCommandStatus::Pending => {}
            _ => {
                tracing::info!(chat = %handle.chat_id, command = %command_id,
                    status = ?status, resolution = resolution.unwrap_or(""),
                    "command resolved without executing");
            }
        }
        if let Err(err) = handle
            .doc()
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
                // References resolve HERE, against the checkout this run is
                // about to start in — not where they were picked. That is what
                // makes a file picked on a phone name the same file when the
                // box runs the turn, and the only place an escape can be caught.
                let context = self
                    .apply_context(chat_id, &request.cwd, &entry.context, &mut request.prompt)
                    .await?;
                let harness = self.harness_for(chat_id);
                sessions
                    .dispatch(chat_id, harness, request, Some(message_id.clone()))
                    .await?;
                Ok((SessionCommandStatus::Applied, context))
            }
            SessionCommandPayload::Steer { prompt, message_id } => {
                let mut prompt = prompt.clone();
                let context = self
                    .apply_context(
                        chat_id,
                        &self.checkout_root(chat_id, sessions),
                        &entry.context,
                        &mut prompt,
                    )
                    .await?;
                let prompt = &prompt;
                match sessions.steer(chat_id, prompt, message_id.clone()).await? {
                    SteerOutcome::Accepted => Ok((SessionCommandStatus::Applied, context)),
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
                            Some(match context {
                                Some(context) => format!("queued as new turn; {context}"),
                                None => "queued as new turn".into(),
                            }),
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
                let questions = handle.doc().read_entries().ok().and_then(|entries| {
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
                if let Err(err) = handle.doc().resolve_input(request_id) {
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
            // A follow-up whose turn has come (the drain only reaches this once
            // the plan says so — head of an unpaused queue, nothing running).
            SessionCommandPayload::Queue {
                prompt,
                message_id,
                attachments,
            } => {
                // The PLAN's text, not the entry's: an edit is a later ledger
                // entry, and running the original would run the version the user
                // replaced. Falls back to the entry for a plan that no longer
                // lists it, which the drain should have cancelled instead — a
                // belt-and-braces path, never the normal one.
                let row = handle
                    .queue_view()
                    .rows
                    .into_iter()
                    .find(|row| row.id == entry.id);
                let (mut prompt, context, attachments) = match row {
                    Some(row) => (row.prompt, row.context, row.attachments),
                    None => (prompt.clone(), entry.context.clone(), attachments.clone()),
                };
                // Same config source as the steer→new-turn fallback: the live
                // request when there is one (a parked persistent session is
                // reused — the follow-up becomes its next turn on the warm
                // child), else the chat's workspace row after a restart.
                let request = sessions
                    .last_request(chat_id)
                    .or_else(|| self.request_from_chat_row(chat_id, &prompt));
                let Some(mut request) = request else {
                    return Ok((
                        SessionCommandStatus::Rejected,
                        Some("no run config for the queued follow-up".into()),
                    ));
                };
                let note = self
                    .apply_context(chat_id, &request.cwd, &context, &mut prompt)
                    .await?;
                request.prompt = prompt;
                request.resume = None; // dispatch re-derives the harness session
                request.attachments = attachments; // this row's images, never the previous turn's
                sessions
                    .dispatch(
                        chat_id,
                        self.harness_for(chat_id),
                        request,
                        Some(message_id.clone()),
                    )
                    .await?;
                Ok((
                    SessionCommandStatus::Applied,
                    Some(match note {
                        Some(note) => format!("ran from the queue; {note}"),
                        None => "ran from the queue".into(),
                    }),
                ))
            }
            // A mutation of the plan has no side effect to execute: its effect
            // IS the entry, which every reader of the ledger already folds
            // ([`comet_doc::queue::project`]). Marking it applied is what stops
            // the drain from re-examining it, and what tells the client its
            // optimistic edit landed durably.
            SessionCommandPayload::QueueControl { op } => {
                Ok((SessionCommandStatus::Applied, Some(control_note(op))))
            }
        }
    }

    /// The checkout a chat's `@` references resolve against: the workspace row's
    /// cwd (what the run will actually use), falling back to the live request's.
    fn checkout_root(&self, chat_id: &str, sessions: &SessionsEngine) -> String {
        self.workspace()
            .and_then(|ws| ws.doc().chat(chat_id).ok().flatten())
            .and_then(|chat| chat.cwd)
            .filter(|cwd| !cwd.is_empty())
            .or_else(|| sessions.last_request(chat_id).map(|r| r.cwd))
            .unwrap_or_default()
    }

    /// Resolve an instruction's references against `root`, appending the
    /// staleness note to `prompt`. Returns the one-line account for the ledger.
    ///
    /// `Err` for a reference that escapes the checkout — which fails the command
    /// rather than dropping the reference, because the picker cannot produce one
    /// and a client that did is not a client whose other references should be
    /// trusted into a prompt.
    async fn apply_context(
        &self,
        chat_id: &str,
        root: &str,
        refs: &[ContextRef],
        prompt: &mut String,
    ) -> Result<Option<String>, EngineError> {
        if refs.is_empty() {
            return Ok(None);
        }
        if root.is_empty() {
            return Err(EngineError::Other(
                "context references need a checkout, and this chat has no cwd".into(),
            ));
        }
        let workspace = self.workspace().ok_or_else(|| {
            EngineError::Other("context references require a workspace host".into())
        })?;
        let chat = workspace
            .doc()
            .chat(chat_id)?
            .ok_or_else(|| EngineError::Other("context references require a chat row".into()))?;
        let space_id = chat.space_id.as_deref().ok_or_else(|| {
            EngineError::Other("context references require an owning space".into())
        })?;
        let space = workspace
            .doc()
            .space(space_id)?
            .filter(|space| space.device_id == workspace.device_id())
            .ok_or_else(|| {
                EngineError::Other("context checkout is not owned by this host".into())
            })?;
        let repos =
            self.inner.repos.get().ok_or_else(|| {
                EngineError::Other("context checkout registry is unavailable".into())
            })?;
        let validated_root = repos
            .validated_checkout_root(&space.path, root)
            .await
            .ok_or_else(|| EngineError::Other("context checkout is not registered".into()))?;
        let declared_checkout = chat.checkout_id.or(space.checkout_id);
        let device_id = workspace.device_id().to_string();
        let expected_checkout = crate::context_files::checkout_identity(
            &device_id,
            &validated_root,
            declared_checkout.as_deref(),
        );
        crate::context_files::validate_checkout(refs, Some(&expected_checkout))?;
        let resolved = crate::context_files::resolve(&validated_root, refs)?;
        if let Some(note) = crate::context_files::missing_note(&resolved) {
            prompt.push_str(&note);
        }
        Ok(crate::context_files::resolution_note(&resolved))
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

    fn persist_snapshot(&self, handle: &ChatDocHandle) -> Result<usize, EngineError> {
        let bytes = handle.doc().export_snapshot()?;
        self.inner.store.save_snapshot(&handle.chat_id, &bytes)?;
        handle.journal_semantics();
        handle.snapshot_bytes.store(bytes.len(), Ordering::Relaxed);
        Ok(bytes.len())
    }

    fn persist_command_outcome(
        &self,
        handle: &ChatDocHandle,
        command_id: &str,
        status: SessionCommandStatus,
        resolution: Option<&str>,
    ) -> Result<usize, EngineError> {
        self.persist_command_outcome_with_hook(handle, command_id, status, resolution, || {})
    }

    fn persist_command_outcome_with_hook(
        &self,
        handle: &ChatDocHandle,
        command_id: &str,
        status: SessionCommandStatus,
        resolution: Option<&str>,
        after_status: impl FnOnce(),
    ) -> Result<usize, EngineError> {
        let bytes_len = handle.with_current(|doc| -> Result<usize, EngineError> {
            doc.set_command_status(command_id, status, resolution)?;
            after_status();
            let bytes = doc.export_snapshot()?;
            self.inner.store.save_snapshot(&handle.chat_id, &bytes)?;
            // A command outcome is durable local content like any other: it
            // must be replayable across a shallow-history cut, so it enters
            // the outbox in the same call that makes the snapshot durable.
            // Against the doc THIS closure holds — see `journal_semantics_of`.
            handle.journal_semantics_of(doc);
            Ok(bytes.len())
        })?;
        handle.snapshot_bytes.store(bytes_len, Ordering::Relaxed);
        Ok(bytes_len)
    }

    fn save_snapshot(&self, handle: &ChatDocHandle) {
        if let Err(err) = self.persist_snapshot(handle) {
            tracing::warn!(chat = %handle.chat_id, error = %err, "snapshot save failed");
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

/// What a plan mutation writes into the ledger's `resolution` — the line a
/// client renders next to the entry, and the only trace an op leaves once its
/// effect has been folded into the plan.
fn control_note(op: &comet_doc::QueueOp) -> String {
    use comet_doc::QueueOp;
    match op {
        QueueOp::Edit { target, .. } => format!("edited {target}"),
        QueueOp::Move { target, .. } => format!("reordered {target}"),
        QueueOp::Remove { target } => format!("removed {target}"),
        QueueOp::Clear {} => "cleared the queue".into(),
        QueueOp::Pause {} => "paused the queue".into(),
        QueueOp::Resume {} => "resumed the queue".into(),
        QueueOp::RunNext { target } => format!("run {target} next"),
    }
}

/// Does this command kind get the send TTL?
fn entry_expires(payload: &SessionCommandPayload) -> bool {
    !matches!(
        payload,
        SessionCommandPayload::Queue { .. } | SessionCommandPayload::QueueControl { .. }
    )
}

/// Per-chat background task: reacts to doc changes (local commits and remote imports)
/// by re-publishing the transcript watch, draining commands, and debouncing snapshots.
/// Holds only a weak handle so a dropped host tears the task down.
async fn chat_task(host: DocHost, weak: Weak<ChatDocHandle>, mut changed_rx: watch::Receiver<u64>) {
    // Initial pass: the snapshot may already carry pending commands. The
    // mirror stays lazy — it materializes on the first watch attach.
    {
        let Some(handle) = weak.upgrade() else { return };
        host.reconcile_orphaned_streams(&handle);
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
                handle.publish_queue_if_watched();
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
            rooms_to_release(
                vec![candidate("fresh", now, false)],
                now,
                30_000,
                4,
                NO_BUDGET
            )
            .is_empty(),
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
    async fn a_reseed_moves_the_handle_and_its_watch_to_the_replacement() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = host(dir.path());
        let handle = host.open("chat-reseed").expect("open");
        handle
            .write_user_message("old", "stale prefix", 1)
            .expect("write old");
        let mut messages = handle.watch_messages();
        messages.borrow_and_update();

        let replacement = SessionDoc::init("chat-reseed").expect("replacement doc");
        replacement
            .push_message(&SessionMessageEntry {
                id: "server".into(),
                role: MessageRole::User,
                parts: vec![MessagePart::Text {
                    id: "server-text".into(),
                    text: "complete server transcript".into(),
                }],
                device_id: "server-device".into(),
                created_at: 2,
                status: None,
                continuation_of: None,
            })
            .expect("write replacement");
        handle
            .reseed(replacement.doc().clone())
            .expect("reseed owner");

        tokio::time::timeout(Duration::from_secs(1), messages.changed())
            .await
            .expect("replacement is published")
            .expect("watch stays open");
        assert_eq!(handle.doc().read_entries().unwrap()[0].id, "server");
        assert_eq!(messages.borrow_and_update()[0].id, "server");

        // The reseed also rebinds the root subscription: later writes through
        // the stable handle still wake transcript consumers.
        handle
            .write_user_message("after", "written after reseed", 3)
            .expect("write through replacement");
        tokio::time::timeout(Duration::from_secs(1), messages.changed())
            .await
            .expect("post-reseed commit is published")
            .expect("watch stays open");
        assert_eq!(messages.borrow().len(), 2);
    }

    #[tokio::test]
    async fn a_command_queued_at_the_reseed_boundary_survives_the_owner_swap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = host(dir.path());
        let handle = host.open("chat-reseed-command").expect("open");
        let replacement = SessionDoc::init("chat-reseed-command").expect("replacement doc");
        let (queue_entered_tx, queue_entered_rx) = std::sync::mpsc::channel();
        let (release_queue_tx, release_queue_rx) = std::sync::mpsc::channel();
        let queue_handle = handle.clone();
        let queue = std::thread::spawn(move || {
            queue_handle.with_current(|stale| {
                queue_entered_tx.send(()).unwrap();
                release_queue_rx.recv().unwrap();
                stale
                    .queue_command(&SessionCommandEntry {
                        id: "boundary-command".into(),
                        payload: SessionCommandPayload::Interrupt {},
                        context: Vec::new(),
                        issued_by: queue_handle.device_id.clone(),
                        issued_at: 1,
                        based_on: None,
                        expires_at: None,
                        status: SessionCommandStatus::Pending,
                        resolution: None,
                    })
                    .expect("queue at boundary");
            });
        });
        queue_entered_rx.recv().unwrap();

        // Replacement races the retained-old-owner commit but must wait on the
        // same gate. Once the queue releases it, the final replay observes the
        // command and swaps only after copying it.
        let (replace_started_tx, replace_started_rx) = std::sync::mpsc::channel();
        let (replace_done_tx, replace_done_rx) = std::sync::mpsc::channel();
        let replace_handle = handle.clone();
        let replace = std::thread::spawn(move || {
            replace_started_tx.send(()).unwrap();
            replace_handle
                .reseed(replacement.doc().clone())
                .expect("reseed owner with boundary command");
            replace_done_tx.send(()).unwrap();
        });
        replace_started_rx.recv().unwrap();
        assert!(
            replace_done_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "reseed waits on command gate"
        );
        release_queue_tx.send(()).unwrap();
        queue.join().unwrap();
        replace.join().unwrap();
        replace_done_rx.recv().unwrap();

        assert_eq!(
            handle.doc().read_commands().unwrap()[0].id,
            "boundary-command",
            "the owner swap must reconcile the final old-doc command delta"
        );
    }

    #[tokio::test]
    async fn a_terminal_outcome_crossing_reseed_never_reopens_pending_and_processed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first_host = host(dir.path());
        let chat_id = "chat-outcome-reseed";
        let command_id = "outcome-reseed";
        first_host
            .queue_command_with_id(
                chat_id,
                command_id,
                SessionCommandPayload::Queue {
                    prompt: "already dispatched".into(),
                    message_id: "outcome-reseed-message".into(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        let handle = first_host.open(chat_id).unwrap();

        // The incoming room snapshot was cut while the command was Pending.
        let replacement = SessionDoc::init(chat_id).unwrap();
        let pending = handle
            .doc()
            .read_commands()
            .unwrap()
            .into_iter()
            .find(|command| command.id == command_id)
            .unwrap();
        replacement.queue_command(&pending).unwrap();

        let (outcome_written_tx, outcome_written_rx) = std::sync::mpsc::channel();
        let (release_outcome_tx, release_outcome_rx) = std::sync::mpsc::channel();
        let outcome_host = first_host.clone();
        let outcome_handle = handle.clone();
        let outcome = std::thread::spawn(move || {
            outcome_host
                .persist_command_outcome_with_hook(
                    &outcome_handle,
                    command_id,
                    SessionCommandStatus::Applied,
                    Some("dispatched"),
                    || {
                        outcome_written_tx.send(()).unwrap();
                        release_outcome_rx.recv().unwrap();
                    },
                )
                .unwrap();
            outcome_host.inner.store.mark_processed(command_id).unwrap();
        });
        outcome_written_rx.recv().unwrap();

        let (reseed_done_tx, reseed_done_rx) = std::sync::mpsc::channel();
        let reseed_handle = handle.clone();
        let reseed = std::thread::spawn(move || {
            reseed_handle.reseed(replacement.doc().clone()).unwrap();
            reseed_done_tx.send(()).unwrap();
        });
        assert!(
            reseed_done_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "reseed waits until status mutation and exact snapshot save finish"
        );
        release_outcome_tx.send(()).unwrap();
        outcome.join().unwrap();
        reseed.join().unwrap();
        reseed_done_rx.recv().unwrap();

        let current = handle.doc().read_commands().unwrap();
        assert!(current.iter().any(|command| {
            command.id == command_id
                && command.status == SessionCommandStatus::Applied
                && command.resolution.as_deref() == Some("dispatched")
        }));
        first_host.persist_snapshot(&handle).unwrap();

        let restarted = host(dir.path());
        let reopened = restarted.open(chat_id).unwrap();
        let commands = reopened.doc().read_commands().unwrap();
        assert!(commands.iter().any(|command| {
            command.id == command_id
                && command.status == SessionCommandStatus::Applied
                && command.resolution.as_deref() == Some("dispatched")
        }));
        assert!(restarted.inner.store.is_processed(command_id).unwrap());
        assert!(!commands.iter().any(|command| {
            command.id == command_id && command.status == SessionCommandStatus::Pending
        }));
    }

    #[tokio::test]
    async fn an_absent_terminal_command_crossing_reseed_survives_processed_and_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first_host = host(dir.path());
        let chat_id = "chat-absent-outcome-reseed";
        let command_id = "absent-outcome-reseed";
        first_host
            .queue_command_with_id(
                chat_id,
                command_id,
                SessionCommandPayload::Queue {
                    prompt: "already dispatched before server saw it".into(),
                    message_id: "absent-outcome-message".into(),
                    attachments: Vec::new(),
                },
            )
            .unwrap();
        let handle = first_host.open(chat_id).unwrap();

        // This incoming snapshot predates the command entirely, not merely its
        // outcome. It must receive the locally issued terminal row at reseed.
        let replacement = SessionDoc::init(chat_id).unwrap();
        let (outcome_written_tx, outcome_written_rx) = std::sync::mpsc::channel();
        let (release_outcome_tx, release_outcome_rx) = std::sync::mpsc::channel();
        let outcome_host = first_host.clone();
        let outcome_handle = handle.clone();
        let outcome = std::thread::spawn(move || {
            outcome_host
                .persist_command_outcome_with_hook(
                    &outcome_handle,
                    command_id,
                    SessionCommandStatus::Applied,
                    Some("dispatched"),
                    || {
                        outcome_written_tx.send(()).unwrap();
                        release_outcome_rx.recv().unwrap();
                    },
                )
                .unwrap();
            outcome_host.inner.store.mark_processed(command_id).unwrap();
        });
        outcome_written_rx.recv().unwrap();

        let (reseed_done_tx, reseed_done_rx) = std::sync::mpsc::channel();
        let reseed_handle = handle.clone();
        let reseed = std::thread::spawn(move || {
            reseed_handle.reseed(replacement.doc().clone()).unwrap();
            reseed_done_tx.send(()).unwrap();
        });
        assert!(
            reseed_done_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "missing-snapshot reseed waits for the exact terminal snapshot"
        );
        release_outcome_tx.send(()).unwrap();
        outcome.join().unwrap();
        reseed.join().unwrap();
        reseed_done_rx.recv().unwrap();

        assert!(first_host.inner.store.is_processed(command_id).unwrap());
        let current = handle.doc().read_commands().unwrap();
        assert!(current.iter().any(|command| {
            command.id == command_id
                && command.status == SessionCommandStatus::Applied
                && command.resolution.as_deref() == Some("dispatched")
        }));
        first_host.persist_snapshot(&handle).unwrap();

        let restarted = host(dir.path());
        let commands = restarted
            .open(chat_id)
            .unwrap()
            .doc()
            .read_commands()
            .unwrap();
        assert!(commands.iter().any(|command| {
            command.id == command_id
                && command.status == SessionCommandStatus::Applied
                && command.resolution.as_deref() == Some("dispatched")
        }));
        assert!(restarted.inner.store.is_processed(command_id).unwrap());
        assert!(!commands.iter().any(|command| {
            command.id == command_id && command.status == SessionCommandStatus::Pending
        }));
    }

    #[tokio::test]
    async fn typed_context_reaches_execution_validation_after_snapshot_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first_host = host(dir.path());
        let chat_id = "chat-context-restart";
        let context = vec![ContextRef {
            path: "src/main.rs".into(),
            kind: comet_proto::ContextRefKind::File,
            checkout_id: Some("host-checkout".into()),
        }];
        first_host
            .queue_command_with_id_and_context(
                chat_id,
                "context-restart",
                SessionCommandPayload::Steer {
                    prompt: "inspect @src/main.rs".into(),
                    message_id: None,
                },
                context.clone(),
            )
            .unwrap();
        let handle = first_host.open(chat_id).unwrap();
        first_host.persist_snapshot(&handle).unwrap();

        let restarted = host(dir.path());
        let entry = restarted
            .open(chat_id)
            .unwrap()
            .doc()
            .read_commands()
            .unwrap()
            .into_iter()
            .find(|command| command.id == "context-restart")
            .unwrap();
        assert_eq!(entry.context, context, "snapshot preserves typed authority");
        let mut prompt = "inspect @src/main.rs".to_string();
        let err = restarted
            .apply_context(chat_id, "/registered/checkout", &entry.context, &mut prompt)
            .await
            .expect_err("the restored non-empty context reaches host validation");
        assert!(
            err.to_string().contains("workspace host"),
            "validation was reached after restart: {err}"
        );
    }

    #[tokio::test]
    async fn a_caller_owned_command_id_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = host(dir.path());
        let queued = SessionCommandPayload::Queue {
            prompt: "first".into(),
            message_id: "message-first".into(),
            attachments: Vec::new(),
        };
        host.queue_command_with_id_and_context(
            "chat-fixed-command",
            "prep-release-1",
            queued.clone(),
            Vec::new(),
        )
        .unwrap();
        // The append committed but its response was lost. Another client's
        // operation lands before the original gesture retries.
        host.queue_command_with_id(
            "chat-fixed-command",
            "intervening-control",
            SessionCommandPayload::QueueControl {
                op: comet_doc::QueueOp::Pause {},
            },
        )
        .unwrap();
        host.queue_command_with_id_and_context(
            "chat-fixed-command",
            "prep-release-1",
            queued,
            Vec::new(),
        )
        .expect("same gesture retry is acknowledged without a second append");
        let commands = host
            .open("chat-fixed-command")
            .unwrap()
            .doc()
            .read_commands()
            .unwrap();
        assert_eq!(
            commands
                .iter()
                .filter(|command| command.id == "prep-release-1")
                .count(),
            1
        );
        assert_eq!(
            commands.len(),
            2,
            "retry did not append after intervening work"
        );

        let collision = host.queue_command_with_id(
            "chat-fixed-command",
            "prep-release-1",
            SessionCommandPayload::Interrupt {},
        );
        assert!(
            collision
                .expect_err("same id cannot acknowledge different intent")
                .to_string()
                .contains("collision")
        );
    }

    #[tokio::test]
    async fn a_durable_command_is_in_a_fresh_hosts_snapshot_before_acknowledgement() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = host(dir.path());
        first
            .queue_command_with_id_and_context_durable(
                "chat-durable-command",
                "prep-release-durable",
                SessionCommandPayload::Queue {
                    prompt: "survive acknowledged crash".into(),
                    message_id: "durable-message".into(),
                    attachments: Vec::new(),
                },
                Vec::new(),
            )
            .expect("durable follow-up acknowledged");

        // Do not flush or wait for the ordinary debounce. A new host reading
        // only the store models a crash immediately after the acknowledgement.
        let restarted = host(dir.path());
        let commands = restarted
            .open("chat-durable-command")
            .unwrap()
            .doc()
            .read_commands()
            .unwrap();
        assert!(commands.iter().any(|command| {
            command.id == "prep-release-durable" && command.status == SessionCommandStatus::Pending
        }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_queued_dispatch_snapshots_its_terminal_outcome_before_processed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data = dir.path().join("data");
        let command_id = "queue-crash-after-outcome";
        let chat_id = "chat-queue-crash-after-outcome";
        let assemble = || {
            let registry = crate::HarnessRegistry::new();
            registry.register(Arc::new(comet_harness::mock::MockHarness {
                script: vec![comet_proto::AgentEvent::Done {
                    status: comet_proto::DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: None,
                }],
            }));
            crate::EngineCore::assemble(&data, Arc::new(registry), HarnessId::Mock, None)
                .expect("engine assembles")
        };

        let core = assemble();
        core.workspace
            .create_space(
                "space-queue-outcome",
                &core.device_id,
                &dir.path().to_string_lossy(),
                None,
                false,
            )
            .unwrap();
        core.workspace
            .create_chat(chat_id, "space-queue-outcome", None, None)
            .unwrap();
        *lock(FAIL_AFTER_OUTCOME_PERSIST.get_or_init(|| Mutex::new(None))) =
            Some(command_id.into());
        core.doc_host
            .queue_command_with_id_and_context_durable(
                chat_id,
                command_id,
                SessionCommandPayload::Queue {
                    prompt: "one durable follow-up".into(),
                    message_id: "one-durable-message".into(),
                    attachments: Vec::new(),
                },
                Vec::new(),
            )
            .expect("queue acknowledgement is durable");

        let handle = core.doc_host.open(chat_id).unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if handle.doc().read_commands().unwrap().iter().any(|command| {
                command.id == command_id && command.status == SessionCommandStatus::Applied
            }) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "outcome crash point not reached"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!core.doc_host.inner.store.is_processed(command_id).unwrap());

        core.shutdown().await;
        drop(core);
        let restarted = assemble();
        let reopened = restarted
            .doc_host
            .open(chat_id)
            .expect("terminal outcome reopens");
        restarted.doc_host.drain_commands(&reopened).await;
        let commands = reopened.doc().read_commands().unwrap();
        assert!(commands.iter().any(|command| {
            command.id == command_id && command.status == SessionCommandStatus::Applied
        }));
        assert_eq!(
            reopened
                .doc()
                .read_entries()
                .unwrap()
                .iter()
                .filter(|entry| entry.role == MessageRole::User)
                .count(),
            1,
            "a terminal queued command is not dispatched again after restart"
        );
        restarted.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_crash_before_run_dispatch_leaves_the_durable_turn_for_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let data = dir.path().join("data");
        let command_id = "run-crash-before-dispatch";
        let chat_id = "chat-run-crash-before-dispatch";

        let assemble = || {
            let registry = crate::HarnessRegistry::new();
            registry.register(Arc::new(comet_harness::mock::MockHarness {
                script: vec![comet_proto::AgentEvent::Done {
                    status: comet_proto::DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: None,
                }],
            }));
            crate::EngineCore::assemble(&data, Arc::new(registry), HarnessId::Mock, None)
                .expect("engine assembles")
        };

        let core = assemble();
        *lock(FAIL_BEFORE_RUN_DISPATCH.get_or_init(|| Mutex::new(None))) = Some(command_id.into());
        core.doc_host
            .queue_command_with_id_durable(
                chat_id,
                command_id,
                SessionCommandPayload::Run {
                    request: comet_proto::RunRequest {
                        prompt: "survive the crash".into(),
                        model: None,
                        reasoning: None,
                        model_options: Default::default(),
                        cwd: dir.path().to_string_lossy().into_owned(),
                        sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
                        auto_approve: true,
                        attachments: Vec::new(),
                        resume: None,
                    },
                    message_id: "message-after-restart".into(),
                },
            )
            .expect("durable Run is queued");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !lock(&core.doc_host.inner.starting_runs).contains(command_id) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "crash point was never reached"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !core.doc_host.inner.store.is_processed(command_id).unwrap(),
            "Run was made unrecoverable before Sessions owned it"
        );
        assert!(
            core.doc_host
                .open(chat_id)
                .unwrap()
                .doc()
                .read_commands()
                .unwrap()
                .iter()
                .any(|command| {
                    command.id == command_id && command.status == SessionCommandStatus::Pending
                })
        );

        core.shutdown().await;
        drop(core);

        let restarted = assemble();
        let handle = restarted.doc_host.open(chat_id).expect("snapshot reopens");
        restarted.doc_host.drain_commands(&handle).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let commands = handle.doc().read_commands().unwrap();
            if commands.iter().any(|command| {
                command.id == command_id && command.status == SessionCommandStatus::Applied
            }) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "restarted host did not recover the Pending Run: {commands:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            restarted
                .doc_host
                .inner
                .store
                .is_processed(command_id)
                .unwrap(),
            "the recovered start was not claimed after Sessions accepted it"
        );
        let entries = handle.doc().read_entries().unwrap();
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.role == MessageRole::User)
                .count(),
            1,
            "the recovered command produced one paid turn"
        );
        restarted.shutdown().await;
    }

    #[tokio::test]
    async fn a_purged_chat_loses_handle_and_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let host = host(dir.path());

        let handle = host.open("chat-doomed").expect("open");
        handle
            .write_user_message("m1", "gone soon", 1)
            .expect("write");
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
