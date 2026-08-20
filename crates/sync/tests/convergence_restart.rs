//! gh#483 §6 — a recovery interrupted at ANY checkpoint finishes on restart,
//! and finishes to the same document however many times it is interrupted.
//!
//! The failure this guards is not "recovery is wrong"; it is "recovery stopped
//! halfway and nobody ever came back". A crash between quarantining the stale
//! document and the edge acknowledging the replay leaves a device whose only
//! complete copy of the transcript is a blob in SQLite that nothing reads. So:
//! every phase boundary is a restart here, the process-local state is thrown
//! away each time (a fresh `DocsStore` handle over the same directory, which is
//! precisely what a relaunch gets), and the invariant is checked after each —
//! all 323 stable ids, once each, in order.
//!
//! The room half of the story lives in `crates/sync/src/room/tests.rs`
//! (`a_shallow_reseed_preserves_all_323_entries_for_a_fresh_client`); this file
//! is the crash matrix underneath it, and it runs without a socket on purpose.

use std::sync::Arc;

use comet_doc::{MessagePart, MessageRole, MessageStatus, SessionDoc, SessionMessageEntry};
use comet_sync::convergence::{replay_into, semantic_mutations};
use comet_sync::{ConvergenceRecovery, DocsStore, RecoveryPhase};
use loro::{ExportMode, LoroDoc};

const SHARED: usize = 249;
const OFFLINE: usize = 74;
const DOC_ID: &str = "chat-b5b3c796";

fn entry(id: &str, device: &str, index: usize) -> SessionMessageEntry {
    SessionMessageEntry {
        id: id.to_string(),
        role: if index % 2 == 0 {
            MessageRole::User
        } else {
            MessageRole::Assistant
        },
        parts: vec![MessagePart::Text {
            id: format!("{id}-p0"),
            text: format!("entry {index} body"),
        }],
        created_at: 1_700_000_000_000 + index as i64,
        device_id: device.to_string(),
        status: Some(MessageStatus::Complete),
        continuation_of: None,
    }
}

fn ids(doc: &LoroDoc) -> Vec<String> {
    SessionDoc::from_doc(doc.clone())
        .read_entries()
        .expect("read entries")
        .into_iter()
        .map(|e| e.id)
        .collect()
}

/// `(shallow cloud replica, laptop replica, all 323 ids in order)` — the
/// incident's shape: the cloud holds the 249-id prefix under a shallow root
/// newer than the dependency the laptop's 74 offline appends carry.
fn incident() -> (LoroDoc, LoroDoc, Vec<String>) {
    let cloud = LoroDoc::new();
    cloud.set_peer_id(1).unwrap();
    let cloud_session = SessionDoc::from_doc(cloud.clone());
    let mut expected = Vec::new();
    for index in 0..SHARED {
        let id = format!("shared-{index:04}");
        cloud_session
            .push_message(&entry(&id, "phone-1", index))
            .unwrap();
        expected.push(id);
    }
    let full = cloud.export(ExportMode::Snapshot).unwrap();

    let laptop = LoroDoc::new();
    laptop.import(&full).unwrap();
    laptop.set_peer_id(2).unwrap();
    let laptop_session = SessionDoc::from_doc(laptop.clone());
    for index in 0..OFFLINE {
        let id = format!("offline-{index:04}");
        laptop_session
            .push_message(&entry(&id, "mac-1", SHARED + index))
            .unwrap();
        expected.push(id);
    }

    let marker = cloud.get_text("trim-marker");
    marker.insert(0, "compaction").unwrap();
    cloud.commit();
    marker.delete(0, marker.len_unicode()).unwrap();
    cloud.commit();
    let shallow_bytes = cloud
        .export(ExportMode::shallow_snapshot(&cloud.oplog_frontiers()))
        .unwrap();
    let shallow = LoroDoc::new();
    assert!(shallow.import(&shallow_bytes).unwrap().pending.is_none());
    assert!(shallow.is_shallow());
    let probe = LoroDoc::new();
    probe
        .import(&laptop.export(ExportMode::Snapshot).unwrap())
        .unwrap();
    assert!(
        probe.import(&shallow_bytes).unwrap().pending.is_some(),
        "the fixture must reproduce the unmergeable shallow snapshot"
    );

    (shallow, laptop, expected)
}

/// A relaunch: everything in memory is gone, the directory is all that is left.
fn restart(dir: &std::path::Path) -> (Arc<DocsStore>, ConvergenceRecovery) {
    let store = Arc::new(DocsStore::open(dir).expect("reopen docs store"));
    let recovery = ConvergenceRecovery::new(store.clone(), DOC_ID);
    (store, recovery)
}

/// The device as it stands after a crash: the replacement document that was
/// live at the time, rebuilt from its saved snapshot.
fn reload(store: &DocsStore) -> LoroDoc {
    let doc = LoroDoc::new();
    if let Some(bytes) = store.load_snapshot(DOC_ID).expect("load snapshot") {
        assert!(
            doc.import(&bytes)
                .expect("import snapshot")
                .pending
                .is_none()
        );
    }
    doc
}

fn assert_complete(doc: &LoroDoc, expected: &[String]) {
    let got = ids(doc);
    assert_eq!(got.len(), expected.len(), "entry count");
    assert_eq!(got, expected, "every stable id, once, in order");
    let unique: std::collections::HashSet<&String> = got.iter().collect();
    assert_eq!(unique.len(), got.len(), "no duplication");
}

/// Crash after the quarantine is written and before anything is replaced.
///
/// The worst-looking case and the safest one: the live document is still the
/// laptop's own, so nothing has been lost even if the quarantine is never read.
#[test]
fn a_crash_after_quarantine_resumes_into_the_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let (store, recovery) = restart(dir.path());
    let (cloud, laptop, expected) = incident();
    recovery.record(&laptop).unwrap();

    // Phase 1 only: quarantine, then die.
    let quarantine = laptop.export(ExportMode::Snapshot).unwrap();
    recovery
        .journal()
        .begin_recovery(&comet_sync::RecoveryRecord {
            doc_id: DOC_ID.to_string(),
            phase: RecoveryPhase::Quarantined,
            quarantine,
            server_version: cloud.oplog_vv().encode(),
            expected_ids: expected.clone(),
            started_at: 0,
        })
        .unwrap();
    store
        .save_snapshot(DOC_ID, &cloud.export(ExportMode::Snapshot).unwrap())
        .unwrap();
    drop(recovery);

    // Relaunch: the saved document is the bare 249-entry cloud snapshot, and
    // the resume is the only thing standing between it and 74 lost entries.
    let (store, recovery) = restart(dir.path());
    let live = reload(&store);
    assert_eq!(ids(&live).len(), SHARED, "the crash left the cloud prefix");
    let report = recovery.resume(&live).unwrap().expect("an open recovery");
    assert_eq!(report.restored_messages.len(), OFFLINE);
    assert_complete(&live, &expected);

    // And again — a second crash in the same window changes nothing.
    let (_store, recovery) = restart(dir.path());
    let second = recovery.resume(&live).unwrap().expect("still open");
    assert!(second.is_empty(), "resume is idempotent: {second:?}");
    assert_complete(&live, &expected);
}

/// Crash between each later checkpoint — reseeded, replayed, acknowledged —
/// and the restart still arrives at the same complete document.
#[test]
fn every_checkpoint_survives_an_interruption() {
    for phase in [
        RecoveryPhase::Quarantined,
        RecoveryPhase::Reseeded,
        RecoveryPhase::Replayed,
        RecoveryPhase::Acknowledged,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (store, recovery) = restart(dir.path());
        let (cloud, laptop, expected) = incident();
        recovery.record(&laptop).unwrap();

        // Run the real recovery, then rewind the durable phase to the one under
        // test — exactly what a process death at that checkpoint leaves behind.
        let report = recovery
            .recover(&laptop, &cloud, &cloud.oplog_vv())
            .unwrap();
        assert_eq!(report.restored_messages.len(), OFFLINE, "{phase:?}");
        recovery.journal().advance_recovery(DOC_ID, phase).unwrap();
        store
            .save_snapshot(DOC_ID, &cloud.export(ExportMode::Snapshot).unwrap())
            .unwrap();
        drop(recovery);

        let (store, recovery) = restart(dir.path());
        let live = reload(&store);
        let resumed = recovery.resume(&live).unwrap().expect("open at {phase:?}");
        assert!(
            resumed.is_empty(),
            "the replayed content is already in the saved document at {phase:?}: {resumed:?}"
        );
        assert_complete(&live, &expected);
        assert!(
            recovery.open_recovery().is_some(),
            "the quarantine is retained at {phase:?} until a fresh client proves convergence"
        );

        // Independent verification is the ONLY thing that releases it.
        assert_eq!(
            recovery
                .confirm_independent(&expected[..expected.len() - 1].to_vec())
                .unwrap(),
            Some(vec![expected[expected.len() - 1].clone()]),
            "a fresh client short of one id keeps the quarantine at {phase:?}"
        );
        assert!(recovery.open_recovery().is_some());
        assert_eq!(
            recovery.confirm_independent(&expected).unwrap(),
            Some(Vec::new())
        );
        assert!(recovery.open_recovery().is_none());
    }
}

/// A crash that loses the live document entirely (the snapshot save never
/// landed) still recovers every entry: the quarantine is a complete document,
/// not a delta.
#[test]
fn a_lost_snapshot_is_rebuilt_from_the_quarantine() {
    let dir = tempfile::tempdir().unwrap();
    let (_store, recovery) = restart(dir.path());
    let (cloud, laptop, expected) = incident();
    recovery.record(&laptop).unwrap();
    recovery
        .recover(&laptop, &cloud, &cloud.oplog_vv())
        .unwrap();
    drop(recovery);

    // Nothing was ever saved: the relaunch starts from an EMPTY document.
    let (store, recovery) = restart(dir.path());
    assert!(store.load_snapshot(DOC_ID).unwrap().is_none());
    let live = LoroDoc::new();
    recovery.resume(&live).unwrap().expect("an open recovery");
    assert_complete(&live, &expected);
}

/// Replay is idempotent by stable id, so the same content arriving twice — from
/// the quarantine AND from the room's own backfill — cannot duplicate an entry.
#[test]
fn replaying_content_the_room_already_delivered_changes_nothing() {
    let (cloud, laptop, expected) = incident();
    let mutations = semantic_mutations(&laptop).unwrap();
    let first = replay_into(&cloud, &mutations).unwrap();
    assert_eq!(first.restored_messages.len(), OFFLINE);
    for _ in 0..3 {
        let again = replay_into(&cloud, &mutations).unwrap();
        assert!(again.is_empty(), "{again:?}");
    }
    assert_complete(&cloud, &expected);
}

/// The outbox is durable: a device that commits offline and is killed before it
/// ever connects still knows, on restart, exactly what the edge does not have.
#[test]
fn the_outbox_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (_store, recovery) = restart(dir.path());
    let (_cloud, laptop, _expected) = incident();
    recovery.record(&laptop).unwrap();
    assert_eq!(recovery.pending(), SHARED + OFFLINE);
    drop(recovery);

    let (_store, recovery) = restart(dir.path());
    assert_eq!(
        recovery.pending(),
        SHARED + OFFLINE,
        "the semantic outbox is on disk, not in the process that died"
    );
    let rows = recovery.journal().unacknowledged(DOC_ID).unwrap();
    let restored: SessionMessageEntry = rows
        .iter()
        .find(|row| row.stable_id == "offline-0073")
        .expect("the last offline entry")
        .as_message()
        .unwrap();
    // Full semantic fidelity, not a summary: parts, timestamp, status,
    // provenance all come back byte for byte.
    assert_eq!(
        restored,
        entry("offline-0073", "mac-1", SHARED + OFFLINE - 1)
    );
}
