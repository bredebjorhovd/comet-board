//! `DocsStore` — local SQLite persistence for doc snapshots and the
//! processed-command ledger. Arbitrary command side effects are marked before
//! execution; engine `Run` commands are marked only after Sessions owns the
//! live run, because process death removes that side effect and must leave the
//! paid turn recoverable.

use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

use crate::convergence::{
    ConvergenceError, ConvergenceJournal, RecoveryPhase, RecoveryRecord, SemanticKind,
    SemanticMutation,
};

/// Errors surfaced by [`DocsStore`].
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Ordered, append-only migrations. Each entry runs once inside a transaction;
/// `schema_migrations` records what has been applied.
const MIGRATIONS: &[&str] = &[
    // v1 — snapshots + processed-command ledger
    "CREATE TABLE snapshots (
        doc_id   TEXT PRIMARY KEY,
        bytes    BLOB NOT NULL,
        saved_at INTEGER NOT NULL
     ) STRICT;
     CREATE TABLE processed_commands (
        command_id   TEXT PRIMARY KEY,
        processed_at INTEGER NOT NULL
     ) STRICT;",
    // v2 — the semantic outbox and the recovery quarantine (gh#483).
    //
    // `semantic_journal` holds locally committed transcript entries and
    // commands AS CONTENT, keyed by stable id: a row survives losing the CRDT
    // history that carried it, which is the whole point. Rows are deleted when
    // the edge's advertised version proves it holds them, so the table is an
    // outbox — bounded by what is unacknowledged, not by transcript length.
    //
    // `recovery_state` is one row per document undergoing a shallow-history
    // replacement: the quarantined pre-replacement snapshot plus the phase it
    // reached. It outlives a crash on purpose — a process that dies between
    // quarantine and acknowledgement must be able to finish, and until it has,
    // these bytes are the only complete copy of the document.
    "CREATE TABLE semantic_journal (
        doc_id      TEXT NOT NULL,
        kind        TEXT NOT NULL,
        stable_id   TEXT NOT NULL,
        seq         INTEGER NOT NULL,
        payload     TEXT NOT NULL,
        version     BLOB NOT NULL,
        recorded_at INTEGER NOT NULL,
        PRIMARY KEY (doc_id, kind, stable_id)
     ) STRICT;
     CREATE INDEX semantic_journal_order ON semantic_journal (doc_id, kind, seq);
     CREATE TABLE recovery_state (
        doc_id         TEXT PRIMARY KEY,
        phase          TEXT NOT NULL,
        quarantine     BLOB NOT NULL,
        server_version BLOB NOT NULL,
        expected_ids   TEXT NOT NULL,
        started_at     INTEGER NOT NULL
     ) STRICT;",
];

/// SQLite-backed store under a data directory (`{data_dir}/docs.sqlite3`).
///
/// Holds warm-open doc snapshots (the DO room is authoritative; these make
/// cold starts instant and offline restarts possible) and the command ledger
/// that gives command execution mark-BEFORE-execute idempotence.
pub struct DocsStore {
    conn: Mutex<Connection>,
}

impl DocsStore {
    /// Open (creating directory, database, and schema as needed).
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self, StoreError> {
        let data_dir = data_dir.as_ref();
        std::fs::create_dir_all(data_dir)?;
        let mut conn = Connection::open(data_dir.join("docs.sqlite3"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Latest saved snapshot for `doc_id`, if any.
    pub fn load_snapshot(&self, doc_id: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let bytes = self
            .conn()
            .query_row(
                "SELECT bytes FROM snapshots WHERE doc_id = ?1",
                params![doc_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(bytes)
    }

    /// Save (upsert) the snapshot for `doc_id`.
    pub fn save_snapshot(&self, doc_id: &str, bytes: &[u8]) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO snapshots (doc_id, bytes, saved_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(doc_id) DO UPDATE SET bytes = excluded.bytes, saved_at = excluded.saved_at",
            params![doc_id, bytes, now_ms()],
        )?;
        Ok(())
    }

    /// Delete the snapshot row for `doc_id` (destructive schema breaks: the
    /// legacy `workspace` row is dropped on open). Missing rows are a no-op.
    pub fn delete_snapshot(&self, doc_id: &str) -> Result<(), StoreError> {
        self.conn().execute(
            "DELETE FROM snapshots WHERE doc_id = ?1",
            params![doc_id],
        )?;
        Ok(())
    }

    /// Whether `command_id` has already been claimed for execution.
    pub fn is_processed(&self, command_id: &str) -> Result<bool, StoreError> {
        let hit = self
            .conn()
            .query_row(
                "SELECT 1 FROM processed_commands WHERE command_id = ?1",
                params![command_id],
                |_| Ok(()),
            )
            .optional()?;
        Ok(hit.is_some())
    }

    /// Claim `command_id` for execution — call BEFORE executing (ledger rule:
    /// a crash mid-execution must never re-run the command). Returns `true`
    /// if this call claimed it, `false` if it was already processed.
    pub fn mark_processed(&self, command_id: &str) -> Result<bool, StoreError> {
        let changed = self.conn().execute(
            "INSERT OR IGNORE INTO processed_commands (command_id, processed_at) VALUES (?1, ?2)",
            params![command_id, now_ms()],
        )?;
        Ok(changed > 0)
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        // A poisoned lock only means another thread panicked mid-query; the
        // connection itself is still usable.
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The durable half of [`crate::convergence`]: the semantic outbox and the
/// recovery quarantine, in the same database as the snapshots they protect.
///
/// One database means one fsync discipline for "what this device committed"
/// and "what this device is holding for a recovery" — a snapshot save and the
/// journal rows that account for it cannot end up on opposite sides of a crash
/// in different files.
impl ConvergenceJournal for DocsStore {
    fn record(
        &self,
        doc_id: &str,
        mutations: &[SemanticMutation],
    ) -> Result<usize, ConvergenceError> {
        if mutations.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(journal_error)?;
        let now = now_ms();
        let mut written = 0;
        for mutation in mutations {
            // Rewrite only when the CONTENT changed: an idle doc re-recorded
            // every save would otherwise churn the whole outbox once a second,
            // and re-stamping unchanged rows with the document's current
            // version would make every entry wait on the newest local op.
            let changed = tx
                .execute(
                    "INSERT INTO semantic_journal
                        (doc_id, kind, stable_id, seq, payload, version, recorded_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(doc_id, kind, stable_id) DO UPDATE SET
                        seq = excluded.seq,
                        payload = excluded.payload,
                        version = excluded.version,
                        recorded_at = excluded.recorded_at
                     WHERE semantic_journal.payload <> excluded.payload",
                    params![
                        doc_id,
                        mutation.kind.as_str(),
                        mutation.stable_id,
                        mutation.seq,
                        mutation.payload,
                        mutation.version,
                        now
                    ],
                )
                .map_err(journal_error)?;
            written += changed;
        }
        tx.commit().map_err(journal_error)?;
        Ok(written)
    }

    fn rebase(
        &self,
        doc_id: &str,
        mutations: &[SemanticMutation],
    ) -> Result<usize, ConvergenceError> {
        if mutations.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(journal_error)?;
        let now = now_ms();
        for mutation in mutations {
            // Unconditional: after a replay the content is carried by NEW
            // operations, so its version must move even though its payload did
            // not. See `ConvergenceJournal::rebase`.
            tx.execute(
                "INSERT INTO semantic_journal
                    (doc_id, kind, stable_id, seq, payload, version, recorded_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(doc_id, kind, stable_id) DO UPDATE SET
                    seq = excluded.seq,
                    payload = excluded.payload,
                    version = excluded.version,
                    recorded_at = excluded.recorded_at",
                params![
                    doc_id,
                    mutation.kind.as_str(),
                    mutation.stable_id,
                    mutation.seq,
                    mutation.payload,
                    mutation.version,
                    now
                ],
            )
            .map_err(journal_error)?;
        }
        tx.commit().map_err(journal_error)?;
        Ok(mutations.len())
    }

    fn unacknowledged(&self, doc_id: &str) -> Result<Vec<SemanticMutation>, ConvergenceError> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT kind, stable_id, seq, payload, version
                 FROM semantic_journal WHERE doc_id = ?1 ORDER BY kind, seq",
            )
            .map_err(journal_error)?;
        let rows = stmt
            .query_map(params![doc_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                ))
            })
            .map_err(journal_error)?;
        let mut out = Vec::new();
        for row in rows {
            let (kind, stable_id, seq, payload, version) = row.map_err(journal_error)?;
            let Some(kind) = SemanticKind::parse(&kind) else {
                // A row written by a newer schema: leave it in place (it is
                // unacknowledged content) rather than replaying it wrongly.
                tracing::warn!(%doc_id, %kind, "skipping journal row of unknown kind");
                continue;
            };
            out.push(SemanticMutation {
                kind,
                stable_id,
                payload,
                version,
                seq,
            });
        }
        Ok(out)
    }

    fn acknowledge(
        &self,
        doc_id: &str,
        acked: &loro::VersionVector,
    ) -> Result<usize, ConvergenceError> {
        let covered: Vec<(String, String)> = self
            .unacknowledged(doc_id)?
            .into_iter()
            .filter(|mutation| mutation.acknowledged_by(acked))
            .map(|mutation| (mutation.kind.as_str().to_string(), mutation.stable_id))
            .collect();
        if covered.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn();
        let tx = conn.transaction().map_err(journal_error)?;
        for (kind, stable_id) in &covered {
            tx.execute(
                "DELETE FROM semantic_journal
                 WHERE doc_id = ?1 AND kind = ?2 AND stable_id = ?3",
                params![doc_id, kind, stable_id],
            )
            .map_err(journal_error)?;
        }
        tx.commit().map_err(journal_error)?;
        Ok(covered.len())
    }

    fn begin_recovery(&self, record: &RecoveryRecord) -> Result<(), ConvergenceError> {
        let expected = serde_json::to_string(&record.expected_ids)?;
        self.conn()
            .execute(
                "INSERT INTO recovery_state
                    (doc_id, phase, quarantine, server_version, expected_ids, started_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(doc_id) DO UPDATE SET
                    phase = excluded.phase,
                    quarantine = excluded.quarantine,
                    server_version = excluded.server_version,
                    expected_ids = excluded.expected_ids,
                    started_at = excluded.started_at",
                params![
                    record.doc_id,
                    record.phase.as_str(),
                    record.quarantine,
                    record.server_version,
                    expected,
                    record.started_at
                ],
            )
            .map_err(journal_error)?;
        Ok(())
    }

    fn advance_recovery(&self, doc_id: &str, phase: RecoveryPhase) -> Result<(), ConvergenceError> {
        self.conn()
            .execute(
                "UPDATE recovery_state SET phase = ?2 WHERE doc_id = ?1",
                params![doc_id, phase.as_str()],
            )
            .map_err(journal_error)?;
        Ok(())
    }

    fn open_recovery(&self, doc_id: &str) -> Result<Option<RecoveryRecord>, ConvergenceError> {
        let row = self
            .conn()
            .query_row(
                "SELECT phase, quarantine, server_version, expected_ids, started_at
                 FROM recovery_state WHERE doc_id = ?1",
                params![doc_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(journal_error)?;
        let Some((phase, quarantine, server_version, expected_ids, started_at)) = row else {
            return Ok(None);
        };
        Ok(Some(RecoveryRecord {
            doc_id: doc_id.to_string(),
            // An unreadable phase must not read as "finished": treat it as the
            // earliest phase, which replays from the quarantine.
            phase: RecoveryPhase::parse(&phase).unwrap_or(RecoveryPhase::Quarantined),
            quarantine,
            server_version,
            expected_ids: serde_json::from_str(&expected_ids)?,
            started_at,
        }))
    }

    fn release_recovery(&self, doc_id: &str) -> Result<(), ConvergenceError> {
        self.conn()
            .execute(
                "DELETE FROM recovery_state WHERE doc_id = ?1",
                params![doc_id],
            )
            .map_err(journal_error)?;
        Ok(())
    }
}

fn journal_error(err: rusqlite::Error) -> ConvergenceError {
    ConvergenceError::Journal(err.to_string())
}

fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version    INTEGER PRIMARY KEY,
            applied_at INTEGER NOT NULL
         ) STRICT",
    )?;
    let current: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    for (index, sql) in MIGRATIONS.iter().enumerate() {
        let version = index as i64 + 1;
        if version <= current {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            params![version, now_ms()],
        )?;
        tx.commit()?;
    }
    Ok(())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_roundtrip_and_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();

        assert_eq!(store.load_snapshot("chat-1").unwrap(), None);
        store.save_snapshot("chat-1", b"v1").unwrap();
        assert_eq!(
            store.load_snapshot("chat-1").unwrap().as_deref(),
            Some(&b"v1"[..])
        );
        store.save_snapshot("chat-1", b"v2-longer-bytes").unwrap();
        assert_eq!(
            store.load_snapshot("chat-1").unwrap().as_deref(),
            Some(&b"v2-longer-bytes"[..])
        );
        // Distinct docs do not collide.
        store.save_snapshot("chat-2", b"other").unwrap();
        assert_eq!(
            store.load_snapshot("chat-1").unwrap().as_deref(),
            Some(&b"v2-longer-bytes"[..])
        );
    }

    #[test]
    fn processed_ledger_claims_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();

        assert!(!store.is_processed("cmd-1").unwrap());
        assert!(store.mark_processed("cmd-1").unwrap(), "first mark claims");
        assert!(store.is_processed("cmd-1").unwrap());
        assert!(
            !store.mark_processed("cmd-1").unwrap(),
            "second mark must not re-claim"
        );
    }

    #[test]
    fn reopen_preserves_data_and_migrations_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = DocsStore::open(dir.path()).unwrap();
            store.save_snapshot("chat-1", b"persisted").unwrap();
            store.mark_processed("cmd-1").unwrap();
        }
        let store = DocsStore::open(dir.path()).unwrap(); // re-runs migrate()
        assert_eq!(
            store.load_snapshot("chat-1").unwrap().as_deref(),
            Some(&b"persisted"[..])
        );
        assert!(store.is_processed("cmd-1").unwrap());
        assert!(!store.mark_processed("cmd-1").unwrap());
    }
}
