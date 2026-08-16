//! The phone redials on the same schedule (gh#405).
//!
//! `apps/ios/Comet/Sync/RoomClient.swift` is a second implementation of the
//! room loop this crate owns — it has to be, because no Rust runs on that
//! device — and its own header says so: "Constants mirrored from room.rs".
//! Mirrored by hand, and nothing checked the mirror.
//!
//! gh#396 changed two rules in `room.rs`: a session must outlive
//! `HEALTHY_SESSION` before its end resets the backoff ladder, and every redial
//! wait is jittered. The Swift kept the old rules for a whole release. A phone
//! whose room answered the join and then died therefore redialed four times a
//! second, with no ceiling, on a battery, against an edge that was already
//! failing — which is the exact behaviour the Rust fix had just removed from
//! every other device in the mesh.
//!
//! So this reads both files as text and holds the phone to three numbers and
//! two shapes. It is a text scan on purpose: the Swift half's own runner
//! (`-sync-spec`, `scripts/ios-sync-spec.sh`) needs a simulator and no CI here
//! has one, so it runs when somebody runs it. This runs on every push, which is
//! what a drift that took a release to notice actually needs.
//!
//! **Not a blanket parity claim.** The phone deliberately differs from the
//! desktop elsewhere in that file — it pings at half the rate and tolerates a
//! longer silence, because a radio is a battery — so this covers the three
//! constants gh#405 is about and says nothing about the rest.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root must exist")
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn room_rs() -> String {
    read("crates/sync/src/room.rs")
}

fn backoff_swift() -> String {
    read("apps/ios/Comet/Sync/ReconnectBackoff.swift")
}

fn room_client_swift() -> String {
    read("apps/ios/Comet/Sync/RoomClient.swift")
}

fn session_store_swift() -> String {
    read("apps/ios/Comet/Sync/SessionStore.swift")
}

fn doc_disk_swift() -> String {
    read("apps/ios/Comet/Sync/DocDisk.swift")
}

/// `const NAME: Duration = Duration::from_secs(30);` → 30_000 ms.
fn rust_duration_ms(source: &str, name: &str) -> u64 {
    let line = source
        .lines()
        .find(|l| l.trim_start().starts_with(&format!("const {name}:")))
        .unwrap_or_else(|| panic!("room.rs no longer declares {name}"));
    let call = line
        .split("Duration::from_")
        .nth(1)
        .unwrap_or_else(|| panic!("{name} is not a Duration literal: {line}"));
    let (unit, rest) = call.split_once('(').expect("a from_x( call");
    let digits: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '_')
        .filter(|c| *c != '_')
        .collect();
    let value: u64 = digits
        .parse()
        .unwrap_or_else(|_| panic!("{name} is not a plain literal: {line}"));
    match unit {
        "millis" => value,
        "secs" => value * 1_000,
        other => panic!("{name} uses an unhandled Duration unit: {other}"),
    }
}

/// `static let baseMs = 250` → 250.
fn swift_number(source: &str, name: &str) -> u64 {
    let needle = format!("static let {name}");
    let line = source
        .lines()
        .find(|l| l.trim_start().starts_with(&needle))
        .unwrap_or_else(|| panic!("ReconnectBackoff.swift no longer declares {name}"));
    let rhs = line
        .split_once('=')
        .unwrap_or_else(|| panic!("{name} has no value: {line}"))
        .1;
    let digits: String = rhs
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_digit() || *c == '_')
        .filter(|c| *c != '_')
        .collect();
    digits
        .parse()
        .unwrap_or_else(|_| panic!("{name} is not a plain literal: {line}"))
}

/// The three numbers. A change to any of them in `room.rs` fails here until the
/// phone follows — which is the whole of gh#405.
#[test]
fn the_phone_ladder_is_the_rust_ladder() {
    let rust = room_rs();
    let swift = backoff_swift();

    assert_eq!(
        swift_number(&swift, "baseMs"),
        rust_duration_ms(&rust, "BACKOFF_BASE"),
        "ReconnectBackoff.baseMs must equal room.rs BACKOFF_BASE"
    );
    assert_eq!(
        swift_number(&swift, "capMs"),
        rust_duration_ms(&rust, "BACKOFF_CAP"),
        "ReconnectBackoff.capMs must equal room.rs BACKOFF_CAP"
    );
    assert_eq!(
        swift_number(&swift, "healthySessionNs"),
        rust_duration_ms(&rust, "HEALTHY_SESSION") * 1_000_000,
        "ReconnectBackoff.healthySessionNs must equal room.rs HEALTHY_SESSION"
    );
}

/// The rule, not just its number: if the Rust ever stops gating the reset on a
/// session lifetime, or stops jittering, this test's premise is gone and the
/// phone should be revisited rather than left mirroring something that no
/// longer exists.
#[test]
fn the_rust_still_gates_the_reset_and_jitters_the_wait() {
    let rust = room_rs();
    assert!(
        rust.contains("HEALTHY_SESSION"),
        "room.rs no longer has a healthy-session gate; the iOS port mirrors one"
    );
    assert!(
        rust.contains("jitter::spread"),
        "room.rs no longer jitters its redial; the iOS port mirrors that it does"
    );
}

/// The shape on the phone: one type owns the ladder, and the redial path asks
/// it for both decisions. The defect gh#405 fixed was an assignment of the base
/// rung sitting in the join handler — so the base is nameable in exactly one
/// file, and a second one is the bug growing back.
#[test]
fn the_phone_ladder_has_one_owner() {
    let client = room_client_swift();
    assert!(
        client.contains("backoff.nextDelayMs(healthy:"),
        "RoomClient must draw its redial wait from ReconnectBackoff (jittered)"
    );
    assert!(
        client.contains("ReconnectBackoff.isHealthy(joinedAt:"),
        "RoomClient must decide the reset on the session's lifetime"
    );

    let dir = repo_root().join("apps/ios/Comet/Sync");
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(&dir)
        .expect("read apps/ios/Comet/Sync")
        .flatten()
    {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "swift")
            && path
                .file_name()
                .is_some_and(|n| n != "ReconnectBackoff.swift")
        {
            let text = std::fs::read_to_string(&path).expect("read a Sync source");
            for (ix, line) in text.lines().enumerate() {
                // The comment in RoomClient that names the old defect is
                // prose, not a schedule.
                if line.contains("baseMs") && !line.trim_start().starts_with("//") {
                    offenders.push(format!("{}:{} {}", path.display(), ix + 1, line.trim()));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "the base rung belongs to ReconnectBackoff alone (gh#405); found:\n  {}",
        offenders.join("\n  ")
    );
}

/// gh#450 is another hand-ported room rule, but this time the false-success
/// value comes from Loro itself: importing a shallow snapshot into an older
/// full-history doc returns `ImportStatus.pending` instead of throwing. Keep
/// the iOS implementation pinned to the three safety gates that make replacing
/// a session doc sound: reject pending imports, validate a fresh candidate
/// against the edge's advertised VV, and replay pending commands before the
/// shared owner swaps to that candidate.
#[test]
fn the_phone_reseeds_incomplete_shallow_imports_without_dropping_intent() {
    let client = room_client_swift();
    let store = session_store_swift();
    let disk = doc_disk_swift();

    assert!(
        client.contains("if status.pending == nil"),
        "RoomClient must not treat a non-throwing pending import as complete"
    );
    assert!(
        client.contains("replacement.oplogVv() == serverVv"),
        "a fresh replacement must reach the VV advertised by the edge"
    );
    assert!(
        client.contains("replayUnresolvedLocalCommands(from: doc, into: replacement)"),
        "device-local pending commands must move before replacement"
    );
    assert!(
        client.contains("command[\"issuedBy\"]?.stringValue == localDeviceId")
            && client.contains("command[\"status\"]?.stringValue == \"pending\""),
        "replay must be limited to this device's unresolved intent"
    );
    assert!(
        client.contains("document.replace(with: replacement)"),
        "the room must swap the shared owner, not only its private doc reference"
    );
    assert!(
        client.contains("guard !fullResyncRequested else { return }")
            && client
                .matches("sendJoinLoro(version: [], suppressJoinEffects: joinedLor)")
                .count()
                == 1
            && client.contains("code == .versionUnknown")
            && client.contains("await requestFullResync()"),
        "every full-snapshot fallback must share one per-connection latch"
    );
    assert!(
        store.contains("private let document = RoomDocument()")
            && store.contains("localDeviceId: config.deviceId"),
        "SessionStore must share the swappable document and identify its command author"
    );
    assert!(
        disk.contains("candidateStatus.pending == nil")
            && disk.contains("installedStatus.pending == nil"),
        "a pending disk import must not mutate or be accepted by the live document"
    );
}
