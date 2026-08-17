use comet_doc::{
    MessagePart, MessageRole, MessageStatus, SessionCommandEntry, SessionCommandPayload,
    SessionCommandStatus, SessionDoc, SessionMessageEntry,
};
use comet_sync::{SessionRecoveryError, build_session_recovery};
use loro::{ExportMode, LoroDoc};

fn message(id: &str, text: &str, created_at: i64) -> SessionMessageEntry {
    SessionMessageEntry {
        id: id.into(),
        role: MessageRole::Assistant,
        parts: vec![MessagePart::Text {
            id: format!("part-{id}"),
            text: text.into(),
        }],
        created_at,
        device_id: "mac-1".into(),
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

#[test]
fn shallow_cloud_snapshot_recovers_the_exact_local_transcript_suffix() {
    let original = LoroDoc::new();
    original.set_peer_id(1).unwrap();
    let original_session = SessionDoc::from_doc(original.clone());
    original_session
        .push_message(&message("shared", "already in cloud", 1))
        .unwrap();
    let full_shared_snapshot = original.export(ExportMode::Snapshot).unwrap();

    let local = LoroDoc::new();
    local.import(&full_shared_snapshot).unwrap();
    local.set_peer_id(2).unwrap();
    SessionDoc::from_doc(local.clone())
        .push_message(&message("local-only", "must reach the phone", 2))
        .unwrap();
    let local_snapshot = local.export(ExportMode::Snapshot).unwrap();

    // Advance and then erase unrelated server history. The visible transcript
    // is still the exact local prefix, but the shallow root is now newer than
    // the dependency carried by the local append.
    let trim_marker = original.get_text("trim-marker");
    trim_marker.insert(0, "advance shallow root").unwrap();
    original.commit();
    trim_marker.delete(0, trim_marker.len_unicode()).unwrap();
    original.commit();

    let cloud_snapshot = original
        .export(ExportMode::shallow_snapshot(&original.oplog_frontiers()))
        .unwrap();
    let cloud = LoroDoc::new();
    assert!(cloud.import(&cloud_snapshot).unwrap().pending.is_none());
    assert!(cloud.is_shallow());

    let incompatible_delta = local
        .export(ExportMode::updates(&cloud.oplog_vv()))
        .unwrap();
    assert!(
        cloud.import(&incompatible_delta).is_err(),
        "the fixture must reproduce ImportUpdatesThatDependsOnOutdatedVersion"
    );

    let recovery = build_session_recovery(&cloud_snapshot, &local_snapshot).unwrap();
    assert_eq!(recovery.manifest.authoritative_entries, 1);
    assert_eq!(recovery.manifest.local_entries, 2);
    assert_eq!(recovery.manifest.replayed_entry_ids, ["local-only"]);

    let candidate = LoroDoc::new();
    assert!(
        candidate
            .import(&recovery.snapshot)
            .unwrap()
            .pending
            .is_none()
    );
    assert_eq!(
        SessionDoc::from_doc(candidate).read_entries().unwrap(),
        vec![
            message("shared", "already in cloud", 1),
            message("local-only", "must reach the phone", 2),
        ]
    );

    let independent_cloud = LoroDoc::new();
    independent_cloud.import(&cloud_snapshot).unwrap();
    let status = independent_cloud.import(&recovery.update).unwrap();
    assert!(status.pending.is_none());
    assert_eq!(
        SessionDoc::from_doc(independent_cloud)
            .read_entries()
            .unwrap()
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>(),
        ["shared", "local-only"]
    );
}

#[test]
fn recovery_preserves_the_complete_local_command_suffix() {
    let cloud = LoroDoc::new();
    cloud.set_peer_id(10).unwrap();
    let cloud_session = SessionDoc::from_doc(cloud.clone());
    cloud_session
        .queue_command(&command("shared-command", SessionCommandStatus::Applied))
        .unwrap();
    let full_shared_snapshot = cloud.export(ExportMode::Snapshot).unwrap();

    let local = LoroDoc::new();
    local.import(&full_shared_snapshot).unwrap();
    local.set_peer_id(11).unwrap();
    let local_session = SessionDoc::from_doc(local.clone());
    local_session
        .queue_command(&command("pending-local", SessionCommandStatus::Pending))
        .unwrap();
    local_session
        .queue_command(&command("applied-local", SessionCommandStatus::Applied))
        .unwrap();

    let marker = cloud.get_text("trim-marker");
    marker.insert(0, "advance").unwrap();
    cloud.commit();
    marker.delete(0, marker.len_unicode()).unwrap();
    cloud.commit();
    let cloud_snapshot = cloud
        .export(ExportMode::shallow_snapshot(&cloud.oplog_frontiers()))
        .unwrap();
    let local_snapshot = local.export(ExportMode::Snapshot).unwrap();

    let recovery = build_session_recovery(&cloud_snapshot, &local_snapshot).unwrap();
    assert_eq!(recovery.manifest.authoritative_commands, 1);
    assert_eq!(recovery.manifest.local_commands, 3);
    assert_eq!(
        recovery.manifest.replayed_command_ids,
        ["pending-local", "applied-local"]
    );

    let candidate = LoroDoc::new();
    candidate.import(&recovery.snapshot).unwrap();
    assert_eq!(
        SessionDoc::from_doc(candidate).read_commands().unwrap(),
        vec![
            command("shared-command", SessionCommandStatus::Applied),
            command("pending-local", SessionCommandStatus::Pending),
            command("applied-local", SessionCommandStatus::Applied),
        ]
    );
}

#[test]
fn recovery_advances_shared_command_outcomes_without_reexecuting_them() {
    let cloud = LoroDoc::new();
    cloud.set_peer_id(20).unwrap();
    let cloud_session = SessionDoc::from_doc(cloud.clone());
    cloud_session
        .queue_command(&command("shared-pending", SessionCommandStatus::Pending))
        .unwrap();
    let full_shared_snapshot = cloud.export(ExportMode::Snapshot).unwrap();

    let local = LoroDoc::new();
    local.import(&full_shared_snapshot).unwrap();
    local.set_peer_id(21).unwrap();
    let local_session = SessionDoc::from_doc(local.clone());
    local_session
        .set_command_status("shared-pending", SessionCommandStatus::Applied, None)
        .unwrap();
    local_session
        .queue_command(&command("local-terminal", SessionCommandStatus::Superseded))
        .unwrap();

    let marker = cloud.get_text("trim-marker");
    marker.insert(0, "advance").unwrap();
    cloud.commit();
    marker.delete(0, marker.len_unicode()).unwrap();
    cloud.commit();
    let cloud_snapshot = cloud
        .export(ExportMode::shallow_snapshot(&cloud.oplog_frontiers()))
        .unwrap();
    let local_snapshot = local.export(ExportMode::Snapshot).unwrap();

    let recovery = build_session_recovery(&cloud_snapshot, &local_snapshot).unwrap();
    assert_eq!(
        recovery.manifest.replayed_command_ids,
        ["shared-pending", "local-terminal"]
    );

    let independent_cloud = LoroDoc::new();
    independent_cloud.import(&cloud_snapshot).unwrap();
    assert!(
        independent_cloud
            .import(&recovery.update)
            .unwrap()
            .pending
            .is_none()
    );
    assert_eq!(
        SessionDoc::from_doc(independent_cloud)
            .read_commands()
            .unwrap(),
        vec![
            command("shared-pending", SessionCommandStatus::Applied),
            command("local-terminal", SessionCommandStatus::Superseded),
        ]
    );
}

#[test]
fn recovery_refuses_a_reordered_command_ledger() {
    let cloud = LoroDoc::new();
    let cloud_session = SessionDoc::from_doc(cloud.clone());
    cloud_session
        .queue_command(&command("second", SessionCommandStatus::Applied))
        .unwrap();
    cloud_session
        .queue_command(&command("first", SessionCommandStatus::Applied))
        .unwrap();

    let local = LoroDoc::new();
    let local_session = SessionDoc::from_doc(local.clone());
    local_session
        .queue_command(&command("first", SessionCommandStatus::Applied))
        .unwrap();
    local_session
        .queue_command(&command("second", SessionCommandStatus::Applied))
        .unwrap();

    let Err(error) = build_session_recovery(
        &cloud.export(ExportMode::Snapshot).unwrap(),
        &local.export(ExportMode::Snapshot).unwrap(),
    ) else {
        panic!("reordered command ledger must be refused");
    };
    assert!(matches!(
        error,
        SessionRecoveryError::NotOrderedSubsequence {
            kind: "command ledger"
        }
    ));
}

#[test]
fn recovery_restores_local_commands_at_their_exact_interleaved_positions() {
    let cloud = LoroDoc::new();
    let cloud_session = SessionDoc::from_doc(cloud.clone());
    cloud_session
        .queue_command(&command("first", SessionCommandStatus::Applied))
        .unwrap();
    cloud_session
        .queue_command(&command("last", SessionCommandStatus::Applied))
        .unwrap();

    let local = LoroDoc::new();
    let local_session = SessionDoc::from_doc(local.clone());
    for id in ["first", "local-middle", "last"] {
        local_session
            .queue_command(&command(id, SessionCommandStatus::Applied))
            .unwrap();
    }

    let recovery = build_session_recovery(
        &cloud.export(ExportMode::Snapshot).unwrap(),
        &local.export(ExportMode::Snapshot).unwrap(),
    )
    .unwrap();
    let candidate = LoroDoc::new();
    candidate.import(&recovery.snapshot).unwrap();
    assert_eq!(
        SessionDoc::from_doc(candidate).read_commands().unwrap(),
        [
            command("first", SessionCommandStatus::Applied),
            command("local-middle", SessionCommandStatus::Applied),
            command("last", SessionCommandStatus::Applied),
        ]
    );
}
