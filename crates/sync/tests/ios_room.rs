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

fn composer_swift() -> String {
    read("apps/ios/Comet/Composer/ComposerView.swift")
}

fn doc_disk_swift() -> String {
    read("apps/ios/Comet/Sync/DocDisk.swift")
}

fn convergence_rs() -> String {
    read("crates/sync/src/convergence.rs")
}

fn convergence_swift() -> String {
    read("apps/ios/Comet/Sync/Convergence.swift")
}

/// `let convergenceUnacknowledgedAlertNs: UInt64 = 120_000_000_000` → the
/// number. Top-level `let`, as opposed to the `static let` inside a type that
/// [`swift_number`] reads.
fn swift_named_number(source: &str, name: &str) -> u64 {
    let needle = format!("let {name}");
    let line = source
        .lines()
        .find(|l| l.trim_start().starts_with(&needle))
        .unwrap_or_else(|| panic!("Convergence.swift no longer declares {name}"));
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

/// The Swift test harness cannot replace Loro's document with an insertion
/// failure. Keep the source-level ownership rule explicit: durable append
/// reports a Bool, and every destructive composer transition is gated on it.
///
/// Since gh#535 the composer also holds **staged attachments**, and they are
/// the more expensive half of what a refused write would drop: the bytes have
/// already crossed the relay to the host by then, so clearing them on a write
/// that never landed costs the person an upload, not just a retype. So the
/// pins below check both — the guard exists, and the clears sit on its far
/// side.
#[test]
fn ios_followup_drafts_survive_a_failed_durable_append() {
    let store = session_store_swift();
    let composer = composer_swift();
    assert!(
        store.contains("private func queueCommand(kind: String, payload: [String: Any],")
            && store.contains("context: [ContextRef] = [], expires: Bool = true) -> Bool")
            && store.contains("if queued { nudgeHost() }")
            && store.contains("return queued"),
        "SessionStore must report durable command insertion success"
    );
    // The send plane reports it too: a caller that clears the draft and the
    // staged files on the strength of a send has to know whether it landed.
    assert!(
        store.contains("attachments: [String] = []) -> Bool")
            && store.contains("func sendSteer(prompt: String, context: [ContextRef] = []) -> Bool")
            && store.matches("guard queued else { return false }").count() == 2,
        "sendRun/sendSteer must report whether the durable append landed"
    );
    assert!(
        composer.contains(
            "guard store.queueFollowup(prompt: withAttachments(text: prompt, paths: paths),"
        ) && composer.contains("if store.editFollowup(id: row.id, prompt: editText")
            && composer.contains("saveQueueControl(store.moveFollowup")
            && composer.contains("saveQueueControl(store.removeFollowup")
            && composer.contains("saveQueueControl(store.runNext")
            && composer.contains("saveQueueControl(store.setFollowupsPaused")
            && composer.contains("followupFailure ="),
        "Composer must retain drafts/edits and surface every failed queue write"
    );
    // …and the send path is gated on the same Bool, with its own notice.
    assert!(
        composer.contains("guard deliver(content: prompt, paths: []) else")
            && composer
                .contains("guard deliver(content: withAttachments(text: prompt, paths: paths),")
            && composer.contains("private func deliver(content: String, paths: [String]) -> Bool")
            && composer.contains("sendNotice = refusedNotice"),
        "Composer must gate its send clears on the durable append too"
    );
    // Presence is not the rule — ORDER is. Read each function's own body and
    // require the clear to sit after its guard: a scan of the whole file would
    // pass on a `send()` that cleared first, by finding `queue()`'s clear.
    for (label, signature, guard_anchor) in [
        (
            "send",
            "private func send() {",
            "guard deliver(content: withAttachments(text: prompt, paths: paths),",
        ),
        (
            "queue",
            "private func queue() {",
            "guard store.queueFollowup(prompt: withAttachments(text: prompt, paths: paths),",
        ),
    ] {
        let body = swift_fn_body(&composer, signature);
        let at = body
            .find(guard_anchor)
            .unwrap_or_else(|| panic!("{label}() no longer guards on the durable write"));
        let clear = body
            .find("attachments = []")
            .unwrap_or_else(|| panic!("{label}() no longer clears the staged attachments"));
        assert!(
            at < clear,
            "{label}() clears the staged attachments before the durable write is known to have \
             landed — a refused write would drop files already uploaded to the host, which costs \
             an upload and not just a retype"
        );
    }
}

/// One Swift function body, by brace matching from its signature. Naive on
/// purpose (no string/comment awareness): the two bodies it is pointed at
/// contain no braces inside literals, and a test that silently scanned the
/// wrong span would be worse than one that panics when that stops being true.
fn swift_fn_body<'a>(source: &'a str, signature: &str) -> &'a str {
    let start = source
        .find(signature)
        .unwrap_or_else(|| panic!("ComposerView.swift no longer declares `{signature}`"))
        + signature.len();
    let mut depth = 1usize;
    for (offset, ch) in source[start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &source[start..start + offset];
                }
            }
            _ => {}
        }
    }
    panic!("`{signature}` has no matching close brace");
}

/// `const NAME: Duration = Duration::from_secs(30);` → 30_000 ms. Reads
/// whichever Rust source it is handed (room.rs, convergence.rs).
fn rust_duration_ms(source: &str, name: &str) -> u64 {
    let line = source
        .lines()
        .find(|l| {
            let l = l.trim_start();
            l.starts_with(&format!("const {name}:")) || l.starts_with(&format!("pub const {name}:"))
        })
        .unwrap_or_else(|| panic!("the Rust source no longer declares {name}"));
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
/// rung sitting in the join handler — so the ladder is DECIDED in exactly one
/// file, and a second decision is the bug growing back. Reading
/// `ReconnectBackoff.baseMs` from elsewhere is the opposite of that defect (one
/// source of truth, consulted), and is allowed; assigning from it is not.
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
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") {
                    continue;
                }
                // A QUALIFIED read of the owner's constant is the healthy
                // pattern, not the defect (gh#527's degraded-state threshold
                // is derived from the base rung, which is exactly how a second
                // reader should get it). What gh#405 fixed was an
                // ASSIGNMENT — a second place that decides the ladder — so
                // strip the qualified reads and flag whatever is left, plus
                // any mutation of a rung that is not a fresh declaration.
                let residue = line.replace("ReconnectBackoff.baseMs", "");
                let mutates_the_ladder = line.contains("ReconnectBackoff.baseMs")
                    && line.contains('=')
                    && !trimmed.starts_with("let ")
                    && !trimmed.starts_with("static let ")
                    && !trimmed.starts_with("private static let ");
                if residue.contains("baseMs") || mutates_the_ladder {
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
/// the iOS implementation pinned to the safety gates that make replacing a
/// session doc sound: reject pending imports, validate a fresh candidate
/// against the edge's advertised VV, and — since gh#483 — put every locally
/// committed SEMANTIC entry back on the replacement before the shared owner
/// swaps to it, not merely this device's unresolved command intent.
#[test]
fn the_phone_reseeds_incomplete_shallow_imports_without_dropping_content() {
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
        client.contains("document.replace(with: replacement, recover:")
            && client.contains(
                "convergence.recover(stale: stale, candidate: candidate, serverVersion: serverVv)"
            ),
        "the reseed must go through the convergence module, which quarantines the stale \
         document and replays local semantic content (gh#483)"
    );
    assert!(
        !client.contains("replayUnresolvedLocalCommands"),
        "the command-intent-only replay is the gh#483 regression; it must not come back"
    );
    assert!(
        client.contains("guard let convergence else {")
            && client.contains("refusing to replace the local document"),
        "without a journal there is no quarantine, and refusing is the only safe answer"
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
            && store.contains("localDeviceId: config.deviceId")
            && store.contains("document.withCurrent"),
        "SessionStore command creation must hold the shared document gate"
    );
    assert!(
        client.contains("func replace(with replacement: LoroDoc,")
            && client.contains("recover: (LoroDoc, LoroDoc) -> ReplayReport?")
            && client.contains("func withCurrent<T>"),
        "RoomDocument replacement must make the recovery and owner swap atomic"
    );
    let sync_spec = read("apps/ios/Comet/App/SyncSpecRunner.swift");
    assert!(
        sync_spec.contains("aCommandQueuedAgainstReseedSurvives")
            && sync_spec.contains("queueHasOldOwner")
            && sync_spec.contains("document.replace(with: replacement)"),
        "the phone runtime spec must exercise concurrent queue-vs-reseed ownership"
    );
    assert!(
        disk.contains("candidateStatus.pending == nil")
            && disk.contains("installedStatus.pending == nil"),
        "a pending disk import must not mutate or be accepted by the live document"
    );
}

/// gh#483: the convergence module is the one place either client may replace a
/// document, and the phone has to run the same one.
///
/// It cannot literally share the Rust — no Rust runs on that device — so this
/// holds the Swift copy to the four things the invariant depends on: a durable
/// quarantine before any replacement, a semantic outbox that survives losing
/// the graph, replay that is idempotent by stable id, and a content state that
/// is never inferred from the socket. Plus the two numbers, which is exactly
/// the drift gh#405 caught a release late.
#[test]
fn the_phone_convergence_module_mirrors_the_rust_one() {
    let rust = convergence_rs();
    let swift = convergence_swift();
    let client = room_client_swift();

    // The invariant itself, in both files, so a reader of either finds it.
    for (label, text) in [("Rust", &rust), ("Swift", &swift)] {
        assert!(
            text.contains("acknowledged it and a fresh independent client can retrieve its"),
            "{label} convergence must state the invariant it exists to hold"
        );
    }

    // Phases, spelled the same, because they are written to disk.
    for phase in ["quarantined", "reseeded", "replayed", "acknowledged"] {
        assert!(
            rust.contains(&format!("\"{phase}\"")) && swift.contains(&format!("case {phase}")),
            "recovery phase `{phase}` must exist on both sides"
        );
    }

    // The outbox rules that make acknowledgement safe.
    assert!(
        swift.contains("func rebase(docId: String") && rust.contains("fn rebase("),
        "both journals must re-key rows onto a replacement's version after a replay"
    );
    assert!(
        swift.contains(
            "guard let recorded = try? VersionVector.decode(bytes: version) else { return false }"
        ) && rust.contains("Err(_) => false,"),
        "undecodable version bytes must never read as acknowledged on either side"
    );
    assert!(
        swift.contains("if rows[position].payload == mutation.payload { continue }")
            && rust.contains("row.payload == mutation.payload")
            && read("crates/sync/src/store.rs")
                .contains("WHERE semantic_journal.payload <> excluded.payload"),
        "unchanged content must keep its original version — in both journals on both sides"
    );

    // Replay: idempotent by stable id, and never rolls a richer remote back.
    assert!(
        swift.contains("if sameContent(remote, local) { continue }")
            && swift.contains("report.diverged.append"),
        "the phone's replay must skip equal entries and refuse to roll back richer ones"
    );

    // Independent verification is what releases the quarantine — on both.
    assert!(
        swift.contains("func confirmIndependent") && rust.contains("fn confirm_independent"),
        "a fresh client's read, not the edge's ack, releases the quarantine"
    );
    assert!(
        swift.contains("func resume(into current: LoroDoc)") && rust.contains("pub fn resume("),
        "both sides must finish an interrupted recovery on restart"
    );

    // Content state is not socket state.
    assert!(
        swift.contains("case blockedLocalOnly(unacked: Int, reason: String)")
            && swift.contains("case pending(unacked: Int, stalled: Bool)")
            && swift.contains("case recovering(phase: RecoveryPhase, unacked: Int)")
            && rust.contains("BlockedLocalOnly { unacked: usize, reason: String }"),
        "both sides must expose converged / pending N / recovering / blocked-local-only"
    );
    assert!(
        client.contains("func contentState() -> ConvergenceState"),
        "the phone's room must be able to answer what its CONTENT state is"
    );

    // The two numbers.
    assert_eq!(
        swift_named_number(&swift, "convergenceUnacknowledgedAlertNs"),
        rust_duration_ms(&rust, "UNACKNOWLEDGED_ALERT") * 1_000_000,
        "the unacknowledged-content alert threshold must match room-for-room"
    );
    assert_eq!(
        swift_number(&client, "livenessTickNs") * swift_number(&client, "convergencePollTicks"),
        rust_duration_ms(&room_rs(), "CONVERGENCE_POLL") * 1_000_000,
        "the phone must recompute its content state on the desktop's cadence"
    );

    // And the phone actually exercises it, in the simulator, on the incident's
    // own numbers.
    let sync_spec = read("apps/ios/Comet/App/SyncSpecRunner.swift");
    assert!(
        sync_spec.contains("theIncidentReplaysEveryLocalEntry")
            && sync_spec.contains("incidentFixture(shared: 249, offline: 74)")
            && sync_spec.contains("anInterruptedRecoveryFinishesOnTheNextLaunch")
            && sync_spec.contains("aLiveRoomIsNotAConvergedRoom"),
        "the phone spec must run the 249+74 recovery, a crash restart, and the state rules"
    );
}

#[test]
fn the_phone_parks_instead_of_redialing_malformed_server_versions() {
    let client = room_client_swift();
    assert!(
        client.contains("parkProtocol")
            && client.contains("invalid server version vector; parking room")
            && client.contains("guard !closed, !protocolParked else { return }"),
        "malformed room protocol must trip a finite circuit rather than reconnect"
    );
}

/// gh#527: the phone must SAY when its rooms are dying, and count the deaths
/// the way the other two implementations do.
///
/// The incident was not that the app did not know. It logged "redialing in
/// 30000ms" 22 times and rendered a transcript that read as a conversation
/// nobody had answered yet — and a person cannot tell that from "nothing you
/// type will arrive until this clears". So: the count reaches the UI, the
/// churn rule is the same 30s line everywhere, and the state is exportable,
/// because during the incident nothing could be got off the phone at all.
///
/// Three implementations now count the same thing — `comet_sync::churn` here,
/// `RoomHealth` on the phone, and the socket census on the edge — so the
/// window and the young-session line are pinned across all three.
#[test]
fn the_phone_says_when_its_rooms_are_dying() {
    let health = read("apps/ios/Comet/Sync/RoomHealth.swift");
    let client = room_client_swift();
    let strip = read("apps/ios/Comet/Views/SyncDegradedStrip.swift");
    let session_view = read("apps/ios/Comet/Views/SessionView.swift");
    let edge = read("edge/src/socket-log.ts");

    // The room loop reports its redial decision — the same pair the ladder is
    // decided on, so the banner and the backoff cannot disagree about what
    // happened.
    assert!(
        client.contains("RoomHealth.shared.redialing(room: room, rungMs: rung, joined: joined, healthy: healthy)")
            && client.contains("RoomHealth.shared.joined(room:")
            && client.contains("RoomHealth.shared.opening(room:"),
        "RoomClient must report open/join/redial into the health census"
    );

    // A room that ANSWERS and dies is churn; a room that cannot be reached is
    // a different fault, and the banner has to name which one it is.
    assert!(
        health.contains("if joined && !healthy {"),
        "the phone must count join-then-die separately from unreachable"
    );
    assert!(
        health.contains("var degraded: Bool { laddered > 0 }"),
        "degraded must mean the ladder has CLIMBED, not that a socket dropped"
    );

    // On screen, above the transcript, with the count that used to live only
    // in the log.
    assert!(
        session_view.contains("SyncDegradedStrip()"),
        "the session screen must render the degraded state"
    );
    assert!(
        health.contains("Rooms reconnecting — replies delayed"),
        "the headline must state the consequence, not the mechanism"
    );

    // And it comes off the phone as text.
    assert!(
        strip.contains("Copy diagnostics") && strip.contains("Share diagnostics"),
        "the degraded state must be copyable and shareable from where it is shown"
    );
    assert!(
        health.contains("static func diagnosticsText(snapshot:"),
        "the report must be a pure function of the snapshot, so it can be asserted"
    );

    // The two numbers, across all three implementations.
    let rust_window_ms = rust_duration_ms(&read("crates/sync/src/churn.rs"), "CHURN_WINDOW");
    let swift_window_ms = {
        // `static let churnWindow: TimeInterval = 60 * 60`
        let line = health
            .lines()
            .find(|l| l.trim_start().starts_with("static let churnWindow"))
            .expect("RoomHealth.swift no longer declares churnWindow");
        let rhs = line.split_once('=').expect("a value").1;
        let seconds: u64 = rhs
            .split('*')
            .map(|part| {
                part.trim()
                    .parse::<u64>()
                    .unwrap_or_else(|_| panic!("churnWindow is not a plain product: {line}"))
            })
            .product();
        seconds * 1_000
    };
    assert_eq!(
        swift_window_ms, rust_window_ms,
        "the phone's churn window must match comet_sync::churn::CHURN_WINDOW"
    );
    assert!(
        edge.contains("export const CHURN_WINDOW_MS = 60 * 60 * 1000;"),
        "and the edge's socket census must count over the same hour"
    );
    assert!(
        edge.contains("export const YOUNG_SOCKET_MS = 30_000;")
            && rust_duration_ms(&room_rs(), "HEALTHY_SESSION") == 30_000,
        "a socket that died young on the edge and a session that earned no reset on \
         a client must be the same 30 seconds"
    );
}
