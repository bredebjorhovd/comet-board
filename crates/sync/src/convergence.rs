//! Convergence recovery — the one place a stale local document is replaced by
//! an authoritative shallow snapshot, and the one place locally committed
//! semantic content is put back on top of it.
//!
//! ## Why this module exists (gh#483, regression of gh#450)
//!
//! gh#450 taught the room client to notice an unmergeable server snapshot
//! (`ImportUpdatesThatDependsOnOutdatedVersion`) and reseed from it. What it
//! replayed across that cut was the **command intent** of the local device —
//! nothing else. Ordinary transcript entries were carried only by the CRDT
//! graph being thrown away, so a Mac holding the 249-entry cloud prefix plus 74
//! locally committed entries had exactly two outcomes available: refuse the
//! reseed and stay permanently unmergeable (what 0.8.0 did — transport live,
//! cloud stuck at 249, Mac stuck at 323), or take the reseed and silently drop
//! 74 entries. Neither is acceptable, and the second is worse.
//!
//! ## The invariant
//!
//! > No locally committed semantic entry may be discarded until the edge has
//! > acknowledged it and a fresh independent client can retrieve its stable id.
//!
//! Three mechanisms hold it up, in order of how much they are trusted:
//!
//! 1. **The quarantine.** Before a document is replaced, the stale document is
//!    exported whole and written durably ([`ConvergenceJournal::begin_recovery`]).
//!    It is retained until convergence has been independently verified, so
//!    every phase of a recovery — including a crash between any two of them —
//!    starts from bytes that still contain everything.
//! 2. **The semantic outbox.** Every locally committed transcript entry and
//!    command is journaled as *semantic content* (the entry, with its parts,
//!    timestamps, status, continuation and provenance), not as a CRDT op.
//!    Semantic content survives the loss of the graph that carried it; ops do
//!    not. Rows are dropped only when the edge's version vector proves the edge
//!    has them.
//! 3. **Idempotent replay by stable id.** [`replay_into`] never inserts an id
//!    the target already carries, so replaying twice — after a crash, after a
//!    resumed recovery, after a redundant reseed — converges to the same
//!    document.
//!
//! Retaining more history at the edge makes the reseed rarer. It is not the
//! safety mechanism; this is.
//!
//! ## Shape of a recovery
//!
//! ```text
//! Quarantined ──▶ Reseeded ──▶ Replayed ──▶ Acknowledged ──▶ (verified, released)
//!      │              │            │              │
//!      └──────────────┴────────────┴──────────────┴── crash here: resume() replays
//!                                                     the quarantine into whatever
//!                                                     document is current. Idempotent.
//! ```
//!
//! The phases are durable ([`RecoveryPhase`]); `resume` is what a restart calls
//! and what makes an interrupted recovery finish instead of stranding content.
//! The room drives phases 1–4; independent verification (a *fresh* client that
//! reads the room back) is the owner's job, because only the owner can afford
//! the extra dial — see `comet_engine::doc_host`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use comet_doc::{
    DocError, SessionCommandEntry, SessionCommandStatus, SessionDoc, SessionMessageEntry,
};
use loro::{ExportMode, LoroDoc, LoroEncodeError, LoroError, VersionVector};
use serde::{Deserialize, Serialize};

/// How long a room may stay transport-live with unacknowledged local content
/// before that becomes a reportable fault rather than ordinary lag (gh#483 §7).
///
/// The incident's shape is precisely "live and never converging", so the
/// threshold has to be short enough to catch a session in progress and long
/// enough that a slow uplink pushing a large attachment is not called a fault.
pub const UNACKNOWLEDGED_ALERT: Duration = Duration::from_secs(120);

#[derive(Debug, thiserror::Error)]
pub enum ConvergenceError {
    #[error("loro: {0}")]
    Loro(#[from] LoroError),
    #[error("loro export: {0}")]
    LoroExport(#[from] LoroEncodeError),
    #[error("session document: {0}")]
    Doc(#[from] DocError),
    #[error("journal: {0}")]
    Journal(String),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("recovery verification failed: {0}")]
    Verification(String),
}

// ── the semantic outbox ─────────────────────────────────────────────────────

/// Which list of the session document a journaled mutation belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SemanticKind {
    Message,
    Command,
}

impl SemanticKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Command => "command",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "message" => Some(Self::Message),
            "command" => Some(Self::Command),
            _ => None,
        }
    }
}

/// One locally committed semantic mutation, in the form that survives losing
/// the CRDT history that carried it.
///
/// `payload` is the entry itself as JSON — parts (text, tool calls, input
/// questions, errors, and therefore attachment refs), `createdAt`, `status`,
/// `continuationOf`, and `deviceId`/`issuedBy` provenance all ride along,
/// because replay re-commits the entry rather than reconstructing a summary of
/// it. `version` is the document's oplog version vector at the moment the
/// mutation was recorded: the edge has this mutation once its advertised
/// version includes that vector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticMutation {
    pub kind: SemanticKind,
    pub stable_id: String,
    pub payload: String,
    pub version: Vec<u8>,
    /// Position in the local document's list at record time — replay order.
    pub seq: i64,
}

impl SemanticMutation {
    pub fn message(
        entry: &SessionMessageEntry,
        seq: i64,
        version: &[u8],
    ) -> Result<Self, ConvergenceError> {
        Ok(Self {
            kind: SemanticKind::Message,
            stable_id: entry.id.clone(),
            payload: serde_json::to_string(entry)?,
            version: version.to_vec(),
            seq,
        })
    }

    pub fn command(
        entry: &SessionCommandEntry,
        seq: i64,
        version: &[u8],
    ) -> Result<Self, ConvergenceError> {
        Ok(Self {
            kind: SemanticKind::Command,
            stable_id: entry.id.clone(),
            payload: serde_json::to_string(entry)?,
            version: version.to_vec(),
            seq,
        })
    }

    pub fn as_message(&self) -> Result<SessionMessageEntry, ConvergenceError> {
        Ok(serde_json::from_str(&self.payload)?)
    }

    pub fn as_command(&self) -> Result<SessionCommandEntry, ConvergenceError> {
        Ok(serde_json::from_str(&self.payload)?)
    }

    /// Is this mutation covered by `acked` — i.e. does the edge demonstrably
    /// hold every op the local document had when this content was committed?
    ///
    /// Deliberately conservative: a row is acknowledged only when the whole
    /// local version at record time is included, so a document with one op the
    /// edge refuses reports everything after it as still pending. That is the
    /// incident's state, and over-reporting it is the correct direction to err.
    pub fn acknowledged_by(&self, acked: &VersionVector) -> bool {
        match VersionVector::decode(&self.version) {
            Ok(recorded) => acked.includes_vv(&recorded),
            // Undecodable version bytes must never be read as "the edge has
            // it" — that is the one mistake that discards content.
            Err(_) => false,
        }
    }
}

/// Every semantic mutation the document currently carries, in list order.
///
/// This is how the quarantine is turned back into replayable content, and how
/// a device that has never journaled anything (an upgrade from a build without
/// this module) still recovers everything it holds: the document is its own
/// outbox of last resort.
///
/// SCOPE: the session document's two stable-id lists — `messages` and
/// `commands`. A document without them (the workspace/registry rooms, whose
/// rows are LWW map entries with no per-entry identity to replay by) yields
/// nothing here, and a recovery on one of those rooms is therefore quarantine
/// plus truthful state, with no replay. That is the same behaviour those rooms
/// had before gh#483 — this module adds the quarantine they did not have — and
/// giving them semantic replay means giving their rows stable ids first.
pub fn semantic_mutations(doc: &LoroDoc) -> Result<Vec<SemanticMutation>, ConvergenceError> {
    let version = doc.oplog_vv().encode();
    let session = SessionDoc::from_doc(doc.clone());
    let mut out = Vec::new();
    for (index, entry) in session.read_entries()?.iter().enumerate() {
        out.push(SemanticMutation::message(entry, index as i64, &version)?);
    }
    for (index, command) in session.read_commands()?.iter().enumerate() {
        out.push(SemanticMutation::command(command, index as i64, &version)?);
    }
    Ok(out)
}

/// Stable ids a recovery must still be able to retrieve afterwards.
pub fn stable_ids(doc: &LoroDoc) -> Result<Vec<String>, ConvergenceError> {
    let session = SessionDoc::from_doc(doc.clone());
    let mut ids: Vec<String> = session
        .read_entries()?
        .into_iter()
        .map(|entry| entry.id)
        .collect();
    ids.extend(session.read_commands()?.into_iter().map(|c| c.id));
    Ok(ids)
}

// ── replay ──────────────────────────────────────────────────────────────────

/// What one [`replay_into`] pass did. Every field is a list of stable ids so a
/// log line, a test, and an operator all name the same things.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplayReport {
    /// Entries the target lacked entirely, re-committed in local order.
    pub restored_messages: Vec<String>,
    /// Entries the target held in a TRUNCATED form (a prefix of the local
    /// parts), rewritten from the local copy.
    pub extended_messages: Vec<String>,
    pub restored_commands: Vec<String>,
    /// Commands whose local outcome was written back onto a still-Pending
    /// authoritative row.
    pub resolved_commands: Vec<String>,
    /// Ids where the authoritative copy carries MORE than the local one. The
    /// authoritative copy wins (replaying would roll it back) and the
    /// quarantine keeps the local bytes for an operator.
    pub diverged: Vec<String>,
}

impl ReplayReport {
    pub fn total(&self) -> usize {
        self.restored_messages.len()
            + self.extended_messages.len()
            + self.restored_commands.len()
            + self.resolved_commands.len()
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0 && self.diverged.is_empty()
    }
}

/// Replay `mutations` onto `target`, idempotently, keyed by stable id.
///
/// The rules, in the order they are applied to each mutation:
///
/// - **id absent** — commit the entry verbatim. Order is preserved by feeding
///   mutations in `seq` order; appends land after whatever the authoritative
///   snapshot already carries, which is where locally-later content belongs.
/// - **id present, content equal** — nothing. This is what makes a second pass
///   after a crash a no-op instead of a duplication.
/// - **id present, local carries at least as much** — the authoritative copy is
///   a truncated prefix (the history cut landed mid-stream); rewrite it from
///   the local copy.
/// - **id present, authoritative carries more** — leave it. Rolling a peer's
///   later content back is the one way replay could destroy data, so it does
///   not; the id is reported as diverged and the quarantine holds the local
///   bytes.
pub fn replay_into(
    target: &LoroDoc,
    mutations: &[SemanticMutation],
) -> Result<ReplayReport, ConvergenceError> {
    let session = SessionDoc::from_doc(target.clone());
    let mut entries: HashMap<String, SessionMessageEntry> = session
        .read_entries()?
        .into_iter()
        .map(|entry| (entry.id.clone(), entry))
        .collect();
    let mut commands: HashMap<String, SessionCommandEntry> = session
        .read_commands()?
        .into_iter()
        .map(|command| (command.id.clone(), command))
        .collect();

    let mut ordered: Vec<&SemanticMutation> = mutations.iter().collect();
    ordered.sort_by_key(|mutation| (mutation.kind.as_str(), mutation.seq));

    let mut report = ReplayReport::default();
    for mutation in ordered {
        match mutation.kind {
            SemanticKind::Message => {
                let local = mutation.as_message()?;
                match entries.get(&local.id) {
                    None => {
                        session.push_message(&local)?;
                        report.restored_messages.push(local.id.clone());
                        entries.insert(local.id.clone(), local);
                    }
                    Some(existing) if existing == &local => {}
                    Some(existing) if existing.parts.len() <= local.parts.len() => {
                        if session.overwrite_recovered_message(&local)? {
                            report.extended_messages.push(local.id.clone());
                            entries.insert(local.id.clone(), local);
                        }
                    }
                    Some(_) => report.diverged.push(local.id.clone()),
                }
            }
            SemanticKind::Command => {
                let local = mutation.as_command()?;
                match commands.get(&local.id) {
                    None => {
                        session.queue_command(&local)?;
                        report.restored_commands.push(local.id.clone());
                        commands.insert(local.id.clone(), local);
                    }
                    Some(existing) if existing == &local => {}
                    // Ledger rule 2: an outcome may be written onto a Pending
                    // row; a settled outcome is never overwritten by another.
                    Some(existing)
                        if existing.status == SessionCommandStatus::Pending
                            && local.status != SessionCommandStatus::Pending =>
                    {
                        session.set_command_status(
                            &local.id,
                            local.status,
                            local.resolution.as_deref(),
                        )?;
                        report.resolved_commands.push(local.id.clone());
                        commands.insert(local.id.clone(), local);
                    }
                    Some(_) => report.diverged.push(local.id.clone()),
                }
            }
        }
    }
    Ok(report)
}

// ── durable recovery state ──────────────────────────────────────────────────

/// Durable phase of an in-flight recovery. Ordered: a resumed recovery replays
/// from the quarantine whenever it finds anything below [`Self::Acknowledged`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RecoveryPhase {
    /// The stale document is durably quarantined. Nothing has been replaced.
    Quarantined,
    /// The validated authoritative snapshot is the live document.
    Reseeded,
    /// Local semantic content has been replayed onto it and pushed.
    Replayed,
    /// The edge's advertised version includes the replay. The quarantine is
    /// STILL retained: acknowledgement is the edge's word, and the invariant
    /// asks for a fresh independent client's.
    Acknowledged,
}

impl RecoveryPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quarantined => "quarantined",
            Self::Reseeded => "reseeded",
            Self::Replayed => "replayed",
            Self::Acknowledged => "acknowledged",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "quarantined" => Some(Self::Quarantined),
            "reseeded" => Some(Self::Reseeded),
            "replayed" => Some(Self::Replayed),
            "acknowledged" => Some(Self::Acknowledged),
            _ => None,
        }
    }
}

/// One open recovery, as it survives a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryRecord {
    pub doc_id: String,
    pub phase: RecoveryPhase,
    /// Full snapshot of the document as it stood before replacement.
    pub quarantine: Vec<u8>,
    /// The version the edge advertised for the snapshot being adopted.
    pub server_version: Vec<u8>,
    /// Every stable id that must still be retrievable when this is over.
    pub expected_ids: Vec<String>,
    pub started_at: i64,
}

/// Durable home for the semantic outbox and the recovery checkpoints.
///
/// `DocsStore` is the production implementation; [`MemoryJournal`] is the
/// process-local one used by rooms with no store behind them (tests, and the
/// bare `RoomClient` a viewport builds) — it keeps the same behaviour, minus
/// the surviving-a-restart part, which is exactly what it cannot promise.
pub trait ConvergenceJournal: Send + Sync + 'static {
    /// Upsert semantic content by (doc, kind, stable id). Recording the same
    /// id twice replaces the payload rather than growing the outbox.
    fn record(
        &self,
        doc_id: &str,
        mutations: &[SemanticMutation],
    ) -> Result<usize, ConvergenceError>;

    /// Re-key existing rows onto a new document's version, whether or not
    /// their content changed.
    ///
    /// This is what a completed replay needs and [`Self::record`] must not do.
    /// Replay re-commits the same semantic content as NEW operations on the
    /// replacement document; the abandoned graph's version can never be
    /// acknowledged, because the edge refused precisely those operations. A row
    /// left on it would sit in the outbox forever, reporting an entry as
    /// local-only that the edge demonstrably holds.
    fn rebase(
        &self,
        doc_id: &str,
        mutations: &[SemanticMutation],
    ) -> Result<usize, ConvergenceError>;

    /// Everything this device committed that the edge has not been shown to
    /// hold, in replay order.
    fn unacknowledged(&self, doc_id: &str) -> Result<Vec<SemanticMutation>, ConvergenceError>;

    /// Drop the rows `acked` proves the edge holds. Returns how many.
    fn acknowledge(&self, doc_id: &str, acked: &VersionVector) -> Result<usize, ConvergenceError>;

    /// Open (or replace) the quarantine for this document.
    ///
    /// One row per document, deliberately. A second recovery starting before
    /// the first was verified overwrites the first quarantine — and that is
    /// safe rather than lossy, because what it writes is the CURRENT document,
    /// which already carries everything the first recovery replayed into it.
    /// The chain of quarantines is monotone in content; keeping every link
    /// would only keep older copies of the same entries.
    fn begin_recovery(&self, record: &RecoveryRecord) -> Result<(), ConvergenceError>;
    fn advance_recovery(&self, doc_id: &str, phase: RecoveryPhase) -> Result<(), ConvergenceError>;
    fn open_recovery(&self, doc_id: &str) -> Result<Option<RecoveryRecord>, ConvergenceError>;
    /// Independent convergence has been verified — release the quarantine.
    fn release_recovery(&self, doc_id: &str) -> Result<(), ConvergenceError>;
}

/// Process-local journal. Same rules, no durability — a restart forgets it,
/// which is why the engine wires `DocsStore` instead.
#[derive(Default)]
pub struct MemoryJournal {
    outbox: Mutex<HashMap<String, HashMap<(SemanticKind, String), SemanticMutation>>>,
    recoveries: Mutex<HashMap<String, RecoveryRecord>>,
}

impl MemoryJournal {
    pub fn new() -> Self {
        Self::default()
    }
}

fn guard<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ConvergenceJournal for MemoryJournal {
    fn record(
        &self,
        doc_id: &str,
        mutations: &[SemanticMutation],
    ) -> Result<usize, ConvergenceError> {
        let mut outbox = guard(&self.outbox);
        let rows = outbox.entry(doc_id.to_string()).or_default();
        let mut written = 0;
        for mutation in mutations {
            let key = (mutation.kind, mutation.stable_id.clone());
            // Unchanged content keeps its ORIGINAL version. Re-stamping it with
            // the document's current version on every save would make every
            // entry depend on the newest local op, so one op the edge has not
            // taken yet would hold the whole transcript in the outbox — the
            // count would be true but useless.
            if rows
                .get(&key)
                .is_some_and(|row| row.payload == mutation.payload)
            {
                continue;
            }
            rows.insert(key, mutation.clone());
            written += 1;
        }
        Ok(written)
    }

    fn rebase(
        &self,
        doc_id: &str,
        mutations: &[SemanticMutation],
    ) -> Result<usize, ConvergenceError> {
        let mut outbox = guard(&self.outbox);
        let rows = outbox.entry(doc_id.to_string()).or_default();
        for mutation in mutations {
            rows.insert(
                (mutation.kind, mutation.stable_id.clone()),
                mutation.clone(),
            );
        }
        Ok(mutations.len())
    }

    fn unacknowledged(&self, doc_id: &str) -> Result<Vec<SemanticMutation>, ConvergenceError> {
        let outbox = guard(&self.outbox);
        let mut rows: Vec<SemanticMutation> = outbox
            .get(doc_id)
            .map(|rows| rows.values().cloned().collect())
            .unwrap_or_default();
        rows.sort_by(|a, b| (a.kind.as_str(), a.seq).cmp(&(b.kind.as_str(), b.seq)));
        Ok(rows)
    }

    fn acknowledge(&self, doc_id: &str, acked: &VersionVector) -> Result<usize, ConvergenceError> {
        let mut outbox = guard(&self.outbox);
        let Some(rows) = outbox.get_mut(doc_id) else {
            return Ok(0);
        };
        let before = rows.len();
        rows.retain(|_, mutation| !mutation.acknowledged_by(acked));
        Ok(before - rows.len())
    }

    fn begin_recovery(&self, record: &RecoveryRecord) -> Result<(), ConvergenceError> {
        guard(&self.recoveries).insert(record.doc_id.clone(), record.clone());
        Ok(())
    }

    fn advance_recovery(&self, doc_id: &str, phase: RecoveryPhase) -> Result<(), ConvergenceError> {
        if let Some(record) = guard(&self.recoveries).get_mut(doc_id) {
            record.phase = phase;
        }
        Ok(())
    }

    fn open_recovery(&self, doc_id: &str) -> Result<Option<RecoveryRecord>, ConvergenceError> {
        Ok(guard(&self.recoveries).get(doc_id).cloned())
    }

    fn release_recovery(&self, doc_id: &str) -> Result<(), ConvergenceError> {
        guard(&self.recoveries).remove(doc_id);
        Ok(())
    }
}

// ── the driver ──────────────────────────────────────────────────────────────

/// The room-sync seam's recovery driver: one object that owns the quarantine,
/// the replay, the acknowledgement accounting, and the truthful state.
///
/// Both viewports run the same sequence — the phone's copy is
/// `apps/ios/Comet/Sync/Convergence.swift`, held to this file's shape by
/// `crates/sync/tests/ios_room.rs`.
#[derive(Clone)]
pub struct ConvergenceRecovery {
    journal: Arc<dyn ConvergenceJournal>,
    doc_id: String,
}

impl ConvergenceRecovery {
    pub fn new(journal: Arc<dyn ConvergenceJournal>, doc_id: impl Into<String>) -> Self {
        Self {
            journal,
            doc_id: doc_id.into(),
        }
    }

    pub fn doc_id(&self) -> &str {
        &self.doc_id
    }

    pub fn journal(&self) -> &Arc<dyn ConvergenceJournal> {
        &self.journal
    }

    /// Record locally committed content into the semantic outbox. Called with
    /// whatever the document currently holds; unchanged rows are not rewritten.
    pub fn record(&self, doc: &LoroDoc) -> Result<usize, ConvergenceError> {
        let mutations = semantic_mutations(doc)?;
        self.journal.record(&self.doc_id, &mutations)
    }

    /// Re-key the outbox onto `doc`'s version after a replay — see
    /// [`ConvergenceJournal::rebase`] for why this is not `record`.
    fn rebase(&self, doc: &LoroDoc) -> Result<usize, ConvergenceError> {
        let mutations = semantic_mutations(doc)?;
        self.journal.rebase(&self.doc_id, &mutations)
    }

    /// Phases 1–3: quarantine the stale document, then replay everything the
    /// candidate lacks onto the candidate.
    ///
    /// The caller has already validated that `candidate` is exactly the
    /// document the edge advertised. This function does not swap anything —
    /// the room does that, under its mutation gate, once this returns Ok.
    /// Returning Err leaves the quarantine in place and the stale document
    /// untouched, which is the safe half of the fork.
    pub fn recover(
        &self,
        stale: &LoroDoc,
        candidate: &LoroDoc,
        server_version: &VersionVector,
    ) -> Result<ReplayReport, ConvergenceError> {
        let quarantine = stale.export(ExportMode::Snapshot)?;
        let mut expected = stable_ids(stale)?;
        expected.extend(stable_ids(candidate)?);
        let outbox = self.journal.unacknowledged(&self.doc_id)?;
        expected.extend(outbox.iter().map(|m| m.stable_id.clone()));
        expected.sort();
        expected.dedup();

        let record = RecoveryRecord {
            doc_id: self.doc_id.clone(),
            phase: RecoveryPhase::Quarantined,
            quarantine,
            server_version: server_version.encode(),
            expected_ids: expected.clone(),
            started_at: now_ms(),
        };
        self.journal.begin_recovery(&record)?;

        // The document is the primary source (it holds everything, whoever
        // authored it); the outbox covers content committed by a build that
        // has since lost its document, which is the crash case.
        let mut mutations = semantic_mutations(stale)?;
        let known: std::collections::HashSet<(SemanticKind, String)> = mutations
            .iter()
            .map(|m| (m.kind, m.stable_id.clone()))
            .collect();
        let next_seq = mutations.iter().map(|m| m.seq).max().unwrap_or(-1) + 1;
        for (offset, row) in outbox
            .into_iter()
            .filter(|row| !known.contains(&(row.kind, row.stable_id.clone())))
            .enumerate()
        {
            mutations.push(SemanticMutation {
                seq: next_seq + offset as i64,
                ..row
            });
        }

        self.journal
            .advance_recovery(&self.doc_id, RecoveryPhase::Reseeded)?;
        let report = replay_into(candidate, &mutations)?;
        self.verify(candidate, &record.expected_ids)?;
        // The replayed content is local-only until the edge says otherwise:
        // journal it against the REPLACEMENT's version so acknowledgement is
        // judged on the document that will actually be pushed. The old version
        // belongs to a graph the edge has refused and can never acknowledge.
        self.rebase(candidate)?;
        self.journal
            .advance_recovery(&self.doc_id, RecoveryPhase::Replayed)?;
        Ok(report)
    }

    /// Restart path: finish whatever a crash interrupted.
    ///
    /// Safe to call at every open, and cheap when there is nothing to do (one
    /// lookup). Replays the quarantine into `current` regardless of which phase
    /// died, because replay is idempotent by stable id — a recovery that
    /// crashed after replaying re-verifies and moves on; one that crashed
    /// before it does the work now.
    pub fn resume(&self, current: &LoroDoc) -> Result<Option<ReplayReport>, ConvergenceError> {
        let Some(record) = self.journal.open_recovery(&self.doc_id)? else {
            return Ok(None);
        };
        let quarantined = LoroDoc::new();
        let status = quarantined.import(&record.quarantine)?;
        if status.pending.is_some() {
            return Err(ConvergenceError::Verification(
                "quarantined snapshot is incomplete; refusing to resume from it".into(),
            ));
        }
        let mut mutations = semantic_mutations(&quarantined)?;
        let known: std::collections::HashSet<(SemanticKind, String)> = mutations
            .iter()
            .map(|m| (m.kind, m.stable_id.clone()))
            .collect();
        let next_seq = mutations.iter().map(|m| m.seq).max().unwrap_or(-1) + 1;
        for (offset, row) in self
            .journal
            .unacknowledged(&self.doc_id)?
            .into_iter()
            .filter(|row| !known.contains(&(row.kind, row.stable_id.clone())))
            .enumerate()
        {
            mutations.push(SemanticMutation {
                seq: next_seq + offset as i64,
                ..row
            });
        }
        let report = replay_into(current, &mutations)?;
        self.verify(current, &record.expected_ids)?;
        self.rebase(current)?;
        if record.phase < RecoveryPhase::Replayed {
            self.journal
                .advance_recovery(&self.doc_id, RecoveryPhase::Replayed)?;
        }
        tracing::warn!(
            doc = %self.doc_id,
            phase = record.phase.as_str(),
            replayed = report.total(),
            "resumed an interrupted convergence recovery"
        );
        Ok(Some(report))
    }

    /// The edge advertised `acked`. Drop the outbox rows it proves are held,
    /// and promote a replayed recovery to [`RecoveryPhase::Acknowledged`].
    pub fn acknowledge(&self, acked: &VersionVector) -> Result<usize, ConvergenceError> {
        let dropped = self.journal.acknowledge(&self.doc_id, acked)?;
        if let Some(record) = self.journal.open_recovery(&self.doc_id)?
            && record.phase == RecoveryPhase::Replayed
            && self.journal.unacknowledged(&self.doc_id)?.is_empty()
        {
            self.journal
                .advance_recovery(&self.doc_id, RecoveryPhase::Acknowledged)?;
        }
        Ok(dropped)
    }

    /// A fresh independent client retrieved `observed_ids` from the room.
    /// Releases the quarantine only when every expected id is among them.
    pub fn confirm_independent(
        &self,
        observed_ids: &[String],
    ) -> Result<Option<Vec<String>>, ConvergenceError> {
        let Some(record) = self.journal.open_recovery(&self.doc_id)? else {
            return Ok(None);
        };
        let observed: std::collections::HashSet<&str> =
            observed_ids.iter().map(String::as_str).collect();
        let missing: Vec<String> = record
            .expected_ids
            .iter()
            .filter(|id| !observed.contains(id.as_str()))
            .cloned()
            .collect();
        if !missing.is_empty() {
            tracing::error!(
                doc = %self.doc_id,
                missing = missing.len(),
                first = %missing[0],
                "independent verification is short of the quarantined content; \
                 keeping the quarantine"
            );
            return Ok(Some(missing));
        }
        self.journal.release_recovery(&self.doc_id)?;
        tracing::info!(
            doc = %self.doc_id,
            verified = record.expected_ids.len(),
            "convergence verified by an independent client; quarantine released"
        );
        Ok(Some(Vec::new()))
    }

    /// Unacknowledged semantic mutations right now.
    pub fn pending(&self) -> usize {
        self.journal
            .unacknowledged(&self.doc_id)
            .map(|rows| rows.len())
            .unwrap_or(0)
    }

    pub fn open_recovery(&self) -> Option<RecoveryRecord> {
        self.journal.open_recovery(&self.doc_id).ok().flatten()
    }

    /// Local completeness proof: a document built from nothing but the
    /// candidate's exported bytes still answers for every expected id. This is
    /// the check that a replay actually landed — the INDEPENDENT half of the
    /// invariant is [`Self::confirm_independent`].
    fn verify(&self, candidate: &LoroDoc, expected: &[String]) -> Result<(), ConvergenceError> {
        let fresh = LoroDoc::new();
        let status = fresh.import(&candidate.export(ExportMode::Snapshot)?)?;
        if status.pending.is_some() {
            return Err(ConvergenceError::Verification(
                "the recovered snapshot does not import completely".into(),
            ));
        }
        let present: std::collections::HashSet<String> = stable_ids(&fresh)?.into_iter().collect();
        let missing: Vec<&String> = expected
            .iter()
            .filter(|id| !present.contains(*id))
            .collect();
        if !missing.is_empty() {
            return Err(ConvergenceError::Verification(format!(
                "{} of {} stable ids are missing from the recovered document (first: {})",
                missing.len(),
                expected.len(),
                missing[0]
            )));
        }
        Ok(())
    }
}

// ── truthful per-room state ─────────────────────────────────────────────────

/// What a room can honestly say about its CONTENT, as opposed to its socket.
///
/// The gh#483 incident is exactly the gap between the two: the app reported the
/// room live — it was — while the cloud sat at 249 entries and the Mac at 323,
/// forever. Transport liveness is [`crate::RoomClient::connected`]; this is
/// whether what was typed on this device is anywhere else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvergenceState {
    /// Everything committed here is acknowledged by the edge.
    Converged,
    /// `unacked` semantic mutations are still local-only. `stalled` means that
    /// has been true past [`UNACKNOWLEDGED_ALERT`] on a live socket — ordinary
    /// lag has become a fault worth surfacing (gh#483 §7).
    Pending { unacked: usize, stalled: bool },
    /// A shallow-history recovery is in flight.
    Recovering {
        phase: RecoveryPhase,
        unacked: usize,
    },
    /// The edge will not take this device's content and recovery cannot fix
    /// it. The document is intact locally and nowhere else.
    BlockedLocalOnly { unacked: usize, reason: String },
}

impl Default for ConvergenceState {
    fn default() -> Self {
        Self::Converged
    }
}

impl ConvergenceState {
    pub fn is_converged(&self) -> bool {
        matches!(self, Self::Converged)
    }

    /// Content that exists only on this device.
    pub fn unacked(&self) -> usize {
        match self {
            Self::Converged => 0,
            Self::Pending { unacked, .. }
            | Self::Recovering { unacked, .. }
            | Self::BlockedLocalOnly { unacked, .. } => *unacked,
        }
    }

    /// Does this state need a human? Blocked always; pending only once it has
    /// outlasted the alert threshold.
    pub fn needs_attention(&self) -> bool {
        match self {
            Self::BlockedLocalOnly { .. } => true,
            Self::Pending { stalled, .. } => *stalled,
            _ => false,
        }
    }

    /// One phrase for a status line. Never says "live".
    pub fn label(&self) -> String {
        match self {
            Self::Converged => "converged".into(),
            Self::Pending {
                unacked,
                stalled: false,
            } => format!("pending {unacked}"),
            Self::Pending {
                unacked,
                stalled: true,
            } => format!("pending {unacked} — unacknowledged past the alert threshold"),
            Self::Recovering { phase, unacked } => {
                format!("recovering ({}, {unacked} local-only)", phase.as_str())
            }
            Self::BlockedLocalOnly { unacked, reason } => {
                format!("blocked/local-only ({unacked} local-only: {reason})")
            }
        }
    }
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use comet_doc::{MessagePart, MessageRole, MessageStatus, SessionCommandPayload};

    fn message(id: &str, text: &str, device: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: MessageRole::Assistant,
            parts: vec![MessagePart::Text {
                id: format!("{id}-p0"),
                text: text.into(),
            }],
            created_at: 7,
            device_id: device.into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        }
    }

    fn command(id: &str, status: SessionCommandStatus) -> SessionCommandEntry {
        SessionCommandEntry {
            id: id.into(),
            payload: SessionCommandPayload::Interrupt {},
            context: Vec::new(),
            issued_by: "mac-1".into(),
            issued_at: 3,
            based_on: None,
            expires_at: None,
            status,
            resolution: None,
        }
    }

    fn session(peer: u64) -> SessionDoc {
        let doc = LoroDoc::new();
        doc.set_peer_id(peer).unwrap();
        SessionDoc::from_doc(doc)
    }

    #[test]
    fn replay_restores_full_entries_and_is_idempotent() {
        let local = session(1);
        local
            .push_message(&message("a", "shared", "mac-1"))
            .unwrap();
        local
            .push_message(&message("b", "local only", "mac-1"))
            .unwrap();
        local
            .queue_command(&command("c1", SessionCommandStatus::Pending))
            .unwrap();

        let server = session(2);
        server
            .push_message(&message("a", "shared", "mac-1"))
            .unwrap();

        let mutations = semantic_mutations(local.doc()).unwrap();
        let first = replay_into(server.doc(), &mutations).unwrap();
        assert_eq!(first.restored_messages, ["b"]);
        assert_eq!(first.restored_commands, ["c1"]);

        let second = replay_into(server.doc(), &mutations).unwrap();
        assert!(second.is_empty(), "replay must be idempotent: {second:?}");
        assert_eq!(server.read_entries().unwrap().len(), 2);
        assert_eq!(server.read_commands().unwrap().len(), 1);
        // Full fidelity, not a summary.
        assert_eq!(
            server.read_entries().unwrap()[1],
            message("b", "local only", "mac-1")
        );
    }

    #[test]
    fn a_truncated_authoritative_entry_is_rewritten_from_the_local_copy() {
        let mut full = message("m", "one", "mac-1");
        full.parts.push(MessagePart::Text {
            id: "m-p1".into(),
            text: " two".into(),
        });
        let local = session(1);
        local.push_message(&full).unwrap();

        let server = session(2);
        server.push_message(&message("m", "one", "mac-1")).unwrap();

        let report = replay_into(server.doc(), &semantic_mutations(local.doc()).unwrap()).unwrap();
        assert_eq!(report.extended_messages, ["m"]);
        assert_eq!(server.read_entries().unwrap(), vec![full]);
    }

    #[test]
    fn a_richer_authoritative_entry_is_never_rolled_back() {
        let local = session(1);
        local.push_message(&message("m", "short", "mac-1")).unwrap();

        let mut richer = message("m", "short", "mac-1");
        richer.parts.push(MessagePart::Text {
            id: "m-p1".into(),
            text: " continued elsewhere".into(),
        });
        let server = session(2);
        server.push_message(&richer).unwrap();

        let report = replay_into(server.doc(), &semantic_mutations(local.doc()).unwrap()).unwrap();
        assert_eq!(report.diverged, ["m"]);
        assert_eq!(server.read_entries().unwrap(), vec![richer]);
    }

    #[test]
    fn a_local_outcome_settles_a_pending_authoritative_command() {
        let local = session(1);
        local
            .queue_command(&command("c1", SessionCommandStatus::Applied))
            .unwrap();
        let server = session(2);
        server
            .queue_command(&command("c1", SessionCommandStatus::Pending))
            .unwrap();

        let report = replay_into(server.doc(), &semantic_mutations(local.doc()).unwrap()).unwrap();
        assert_eq!(report.resolved_commands, ["c1"]);
        assert_eq!(
            server.read_commands().unwrap()[0].status,
            SessionCommandStatus::Applied
        );
    }

    #[test]
    fn acknowledgement_drops_only_what_the_edge_demonstrably_holds() {
        let journal = Arc::new(MemoryJournal::new());
        let recovery = ConvergenceRecovery::new(journal.clone(), "chat-1");
        let local = session(1);
        local.push_message(&message("a", "first", "mac-1")).unwrap();
        recovery.record(local.doc()).unwrap();
        let after_first = local.doc().oplog_vv();
        local
            .push_message(&message("b", "second", "mac-1"))
            .unwrap();
        recovery.record(local.doc()).unwrap();
        assert_eq!(recovery.pending(), 2);

        recovery.acknowledge(&after_first).unwrap();
        let left = journal.unacknowledged("chat-1").unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].stable_id, "b");

        recovery.acknowledge(&local.doc().oplog_vv()).unwrap();
        assert_eq!(recovery.pending(), 0);
    }

    #[test]
    fn state_labels_never_call_a_live_socket_converged() {
        assert_eq!(ConvergenceState::Converged.label(), "converged");
        assert_eq!(
            ConvergenceState::Pending {
                unacked: 74,
                stalled: false
            }
            .label(),
            "pending 74"
        );
        assert!(
            ConvergenceState::Pending {
                unacked: 74,
                stalled: true
            }
            .needs_attention()
        );
        assert!(
            ConvergenceState::BlockedLocalOnly {
                unacked: 74,
                reason: "edge refuses this history".into()
            }
            .needs_attention()
        );
        assert_eq!(
            ConvergenceState::Recovering {
                phase: RecoveryPhase::Replayed,
                unacked: 3
            }
            .unacked(),
            3
        );
    }
}
