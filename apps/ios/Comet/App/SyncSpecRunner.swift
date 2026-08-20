// The room's redial decision, asserted — launch with `-sync-spec` (or run
// `scripts/ios-sync-spec.sh`, which does the whole loop).
//
// gh#405: `RoomClient.swift` is a port of `crates/sync/src/room.rs`, and when
// room.rs fixed its reconnect loop (gh#396) the phone kept both defects for a
// release — the ladder reset on the join answer, so a Durable Object that
// accepted the join and then died put the phone in a 250ms redial loop with no
// ceiling, and nothing was jittered, so every room the phone held redialed on
// the same schedule.
//
// The decision now lives in `ReconnectBackoff`, a value type with no socket in
// it, and this is what checks it. Deliberately NOT an end-to-end check: the
// edge is failing requests on the Durable Objects free-tier duration cap, and a
// reconnect loop is exactly the thing that cannot be honestly verified against
// an edge that is down. These are the arithmetic — no network, no session.
//
// A launch-arg runner rather than XCTest for the reason `SpecRunner` gives at
// length: one target, one shared scheme, and a test target means editing the
// two files the operator keeps local changes in. What the simulator cannot
// close is the CI gap, so the OTHER half of this check —
// `crates/sync/tests/ios_room.rs` — reads both sources as text and fails in
// `cargo test` when the constants drift apart again. That one is the guard
// against gh#405 recurring; this one is the guard against the schedule being
// wrong in the first place.

import Foundation
import Loro

@MainActor
enum SyncSpecRunner {
    static var logURL: URL {
        FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("sync-spec.log")
    }

    private static var failures = 0
    private static var checks = 0

    static func log(_ line: String) {
        print("SYNCSPEC: \(line)")
        if let handle = try? FileHandle(forWritingTo: logURL) {
            handle.seekToEndOfFile()
            handle.write(Data("\(line)\n".utf8))
            try? handle.close()
        } else {
            try? Data("\(line)\n".utf8).write(to: logURL)
        }
    }

    /// One assertion. Records the mismatch rather than trapping, so one broken
    /// rule does not hide the others.
    private static func expect<T: Equatable>(_ actual: T, _ wanted: T, _ what: String) {
        checks += 1
        if actual != wanted {
            failures += 1
            log("FAIL \(what)\n       got:  \(actual)\n       want: \(wanted)")
        }
    }

    private static func expectTrue(_ actual: Bool, _ what: String) {
        expect(actual, true, what)
    }

    // MARK: The run

    static func run() {
        try? FileManager.default.removeItem(at: logURL)
        failures = 0
        checks = 0
        log("sync spec start")

        theLadderClimbsToTheCap()
        everyWaitIsJittered()
        aJoinThenDieRoomIsBounded()
        aWorkingSessionEarnsAFreshLadder()
        onlyALastingSessionCounts()
        aCommandQueuedAgainstReseedSurvives()
        theIncidentReplaysEveryLocalEntry()
        anInterruptedRecoveryFinishesOnTheNextLaunch()
        aLiveRoomIsNotAConvergedRoom()
        githubPushTupleRoundTrips()

        log(failures == 0
            ? "OK \(checks) checks, no drift"
            : "FAILED \(failures) of \(checks) checks")
        log("done")
    }

    // MARK: The rules

    /// The rungs themselves: doubling from 250ms, clamped at 30s, and the wait
    /// drawn from the rung BEFORE it doubles. Pinned with the draw forced to
    /// its floor and to its ceiling, so the interval is checked at both ends
    /// rather than sampled.
    private static func theLadderClimbsToTheCap() {
        var floor = ReconnectBackoff()
        let atFloor = (0..<9).map { _ in floor.nextDelayMs(healthy: false, spread: { _ in 0 }) }
        expect(atFloor, [125, 250, 500, 1000, 2000, 4000, 8000, 15000, 15000],
               "the floor of each rung")
        expect(floor.rungMs, ReconnectBackoff.capMs, "the rung clamps at the cap")

        var ceiling = ReconnectBackoff()
        let atCeiling = (0..<9).map { _ in
            ceiling.nextDelayMs(healthy: false, spread: { max(0, $0 - 1) })
        }
        expect(atCeiling, [249, 499, 999, 1999, 3999, 7999, 15999, 29999, 29999],
               "the ceiling of each rung")
        // The interval is half-open at the top: a wait never reaches the rung,
        // so the cap is a real cap and not a value that can be exceeded.
        expectTrue(atCeiling.allSatisfy { $0 < ReconnectBackoff.capMs },
                   "no wait reaches the cap")
    }

    /// The whole point of the second half of gh#396: rooms that failed together
    /// must not redial together. Thirty rooms holding the same rung draw
    /// different waits, and every draw stays inside `[rung/2, rung)`.
    private static func everyWaitIsJittered() {
        var firstWaits: Set<Int> = []
        for _ in 0..<30 {
            var room = ReconnectBackoff()
            firstWaits.insert(room.nextDelayMs(healthy: false))
        }
        // 125 possible values, 30 rooms: a handful of collisions is expected,
        // a handful of DISTINCT values is not — that is the unjittered shape.
        expectTrue(firstWaits.count >= 8,
                   "thirty rooms draw distinct first waits (saw \(firstWaits.count))")
        expectTrue(firstWaits.allSatisfy { (125..<250).contains($0) },
                   "every first wait is in [125, 250): \(firstWaits.sorted())")

        // The same claim at a climbed rung, where the spread is widest and the
        // pile-up worst.
        var climbed = ReconnectBackoff()
        for _ in 0..<6 { _ = climbed.nextDelayMs(healthy: false) }
        var wideWaits: Set<Int> = []
        for _ in 0..<30 {
            var room = climbed
            wideWaits.insert(room.nextDelayMs(healthy: false))
        }
        expectTrue(wideWaits.count >= 20,
                   "a climbed rung spreads too (saw \(wideWaits.count) of 30)")
        expectTrue(wideWaits.allSatisfy { (8000..<16000).contains($0) },
                   "every climbed wait is in [8000, 16000)")
    }

    /// gh#405's defect, as a count. A room whose DO answers the join and then
    /// dies immediately: five virtual minutes of that, and the dials are
    /// bounded because the ladder is allowed to climb.
    private static func aJoinThenDieRoomIsBounded() {
        let windowMs = 5 * 60 * 1000
        var ladder = ReconnectBackoff()
        var elapsed = 0
        var dials = 0
        while elapsed < windowMs {
            // The session itself contributes nothing: joined, then dead. Not
            // healthy, so it earns no reset.
            elapsed += ladder.nextDelayMs(healthy: false)
            dials += 1
        }
        // The unfixed schedule for the same five minutes, for the record: a
        // reset on every join answer and no jitter is a flat 250ms.
        let unfixed = windowMs / ReconnectBackoff.baseMs
        log("join-then-die: \(dials) dials in 5 virtual minutes (was \(unfixed) before gh#405)")
        expectTrue(dials <= 40, "a join-then-die room dials at most 40 times in 5 minutes")
        expectTrue(ladder.rungMs == ReconnectBackoff.capMs,
                   "the ladder reached the cap rather than sitting at the base")
    }

    /// The other direction, which is the one a fix like this breaks if it is
    /// written carelessly: a room that actually worked must not be punished for
    /// the outage that follows it.
    private static func aWorkingSessionEarnsAFreshLadder() {
        var ladder = ReconnectBackoff()
        for _ in 0..<6 { _ = ladder.nextDelayMs(healthy: false) }
        expect(ladder.rungMs, 16000, "the ladder climbed first")

        let wait = ladder.nextDelayMs(healthy: true, spread: { _ in 0 })
        expect(wait, 125, "a working session redials from the base")
        expect(ladder.rungMs, ReconnectBackoff.baseMs * 2,
               "and the ladder itself is back at the bottom")
    }

    /// What counts as a working session: the lifetime, not the join answer.
    private static func onlyALastingSessionCounts() {
        let joined = DispatchTime.now()
        func after(_ ns: UInt64) -> DispatchTime {
            DispatchTime(uptimeNanoseconds: joined.uptimeNanoseconds + ns)
        }

        expect(ReconnectBackoff.isHealthy(joinedAt: nil, now: after(60_000_000_000)), false,
               "a session that never joined is never healthy")
        expect(ReconnectBackoff.isHealthy(joinedAt: joined, now: joined), false,
               "a join answered this instant is not a working session")
        expect(ReconnectBackoff.isHealthy(joinedAt: joined, now: after(29_999_000_000)), false,
               "just under the lifetime is still not")
        expect(ReconnectBackoff.isHealthy(joinedAt: joined,
                                          now: after(ReconnectBackoff.healthySessionNs)), true,
               "exactly the lifetime is")
        expect(ReconnectBackoff.isHealthy(joinedAt: joined, now: after(120_000_000_000)), true,
               "and anything longer is")
        // The subtraction is unsigned; an underflow would read as healthy.
        expect(ReconnectBackoff.isHealthy(joinedAt: after(1_000_000_000), now: joined), false,
               "a clock that ran backwards is not a working session")
    }

    /// gh#450 review: queueing may already hold the old document when recovery
    /// is ready to replace it. Both operations run concurrently here, with the
    /// queue deliberately paused inside the owner gate. Replacement must wait,
    /// replay that final command, and only then publish the new owner.
    private static func aCommandQueuedAgainstReseedSurvives() {
        let document = RoomDocument()
        let queueHasOldOwner = DispatchSemaphore(value: 0)
        let releaseQueue = DispatchSemaphore(value: 0)
        let queueDone = DispatchSemaphore(value: 0)
        let replaceStarted = DispatchSemaphore(value: 0)
        let replaceDone = DispatchSemaphore(value: 0)

        DispatchQueue.global().async {
            document.withCurrent { stale in
                queueHasOldOwner.signal()
                releaseQueue.wait()
                appendSyncSpecCommand(to: stale, id: "boundary", deviceId: "phone-spec")
            }
            queueDone.signal()
        }
        queueHasOldOwner.wait()

        let recovery = specRecovery(docId: "boundary-chat")
        DispatchQueue.global().async {
            replaceStarted.signal()
            let replacement = LoroDoc()
            _ = document.replace(with: replacement) { stale, candidate in
                recovery.recover(stale: stale, candidate: candidate,
                                 serverVersion: candidate.oplogVv())
            }
            replaceDone.signal()
        }
        replaceStarted.wait()
        expect(replaceDone.wait(timeout: .now() + .milliseconds(100)), .timedOut,
               "replacement waits while command creation owns the gate")
        releaseQueue.signal()
        queueDone.wait()
        replaceDone.wait()

        let commands = document.current().getList(id: "commands").getDeepValue().listValue ?? []
        expect(commands.count, 1, "the boundary command crosses the owner swap")
        expect(commands.first?.mapValue?["id"]?.stringValue, "boundary",
               "the recovered command is the one queued against the old owner")
    }

    // MARK: gh#483 — the phone's half of convergence recovery

    /// The incident, on the phone: a room holding the 249-id prefix and a
    /// replica holding those plus 74 entries this device committed while it
    /// could not reach the room. Every one of the 323 stable ids must survive
    /// the replacement, once each, in order — and replaying again must change
    /// nothing.
    private static func theIncidentReplaysEveryLocalEntry() {
        let (cloud, laptop, expected) = incidentFixture(shared: 249, offline: 74)
        expect(Convergence.stableIds(cloud).count, 249, "the room is stuck at the prefix")
        expect(Convergence.stableIds(laptop).count, 323, "the phone holds the whole thing")

        let recovery = specRecovery(docId: "incident-chat")
        recovery.record(laptop)
        expect(recovery.pending, 323, "everything is local-only until the edge says otherwise")

        guard let report = recovery.recover(stale: laptop, candidate: cloud,
                                            serverVersion: cloud.oplogVv()) else {
            expectTrue(false, "recovery refused the server snapshot")
            return
        }
        expect(report.restoredMessages.count, 74, "the offline entries are replayed as CONTENT")
        expect(Convergence.stableIds(cloud), expected, "no loss, no duplication, same order")

        // Full fidelity, not a summary: parts, timestamps, status, provenance.
        let entries = cloud.getList(id: "messages").getDeepValue().listValue ?? []
        let last = entries.last?.mapValue
        expect(last?["id"]?.stringValue, "offline-0073", "the last entry is the last local one")
        expect(last?["deviceId"]?.stringValue, "phone-spec", "provenance survives")
        expect(last?["status"]?.stringValue, "complete", "status survives")
        expect(last?["createdAt"]?.i64Value, 1_700_000_000_000 + 322, "timestamp survives")
        let parts = last?["parts"]?.listValue?.first?.mapValue
        expect(parts?["text"]?.stringValue, "entry 322 body", "the text body survives")

        // Idempotent: a second pass adds nothing.
        let again = Convergence.replayInto(cloud, mutations: Convergence.semanticMutations(laptop))
        expectTrue(again?.isEmpty ?? false, "replay is idempotent by stable id")
        expect(Convergence.stableIds(cloud), expected, "and the document is unchanged")
    }

    /// A crash between quarantine and acknowledgement: the next launch — a
    /// fresh journal instance over the same directory, which is what a relaunch
    /// gets — replays the quarantined content into whatever document is live.
    private static func anInterruptedRecoveryFinishesOnTheNextLaunch() {
        let root = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("convergence-spec-\(UUID().uuidString)", isDirectory: true)
        let (cloud, laptop, expected) = incidentFixture(shared: 20, offline: 7)
        let journal = FileConvergenceJournal(root: root)
        let recovery = ConvergenceRecovery(docId: "restart-chat", journal: journal)
        recovery.record(laptop)
        // Quarantine only, then "die" — nothing has been replayed.
        guard let quarantine = try? laptop.export(mode: .snapshot) else {
            expectTrue(false, "could not export the quarantine")
            return
        }
        journal.beginRecovery(RecoveryRecord(
            docId: "restart-chat", phase: .quarantined, quarantine: quarantine,
            serverVersion: cloud.oplogVv().encode(), expectedIds: expected, startedAt: 0))

        // Relaunch: new journal object, same files, and a live document that is
        // the bare server snapshot.
        let relaunched = ConvergenceRecovery(docId: "restart-chat",
                                             journal: FileConvergenceJournal(root: root))
        let live = LoroDoc()
        if let bytes = try? cloud.export(mode: .snapshot) {
            _ = try? live.importWith(bytes: bytes, origin: "relaunch")
        }
        expect(Convergence.stableIds(live).count, 20, "the crash left only the prefix")
        guard let report = relaunched.resume(into: live) else {
            expectTrue(false, "the relaunch found no open recovery")
            return
        }
        expect(report.restoredMessages.count, 7, "the interrupted replay finished")
        expect(Convergence.stableIds(live), expected, "every stable id is back")

        // The quarantine is retained until a fresh client proves convergence —
        // acknowledgement alone never releases it.
        expectTrue(relaunched.openRecovery != nil, "the quarantine outlives the replay")
        expect(relaunched.confirmIndependent(Array(expected.dropLast())),
               [expected[expected.count - 1]],
               "a fresh client short of one id keeps the quarantine")
        expectTrue(relaunched.openRecovery != nil, "and it is still there")
        expect(relaunched.confirmIndependent(expected), [], "a complete fresh read releases it")
        expectTrue(relaunched.openRecovery == nil, "the quarantine is gone")
        try? FileManager.default.removeItem(at: root)
    }

    /// The state a view renders is about CONTENT. A joined room holding
    /// unacknowledged entries is not converged, and says so.
    private static func aLiveRoomIsNotAConvergedRoom() {
        expect(ConvergenceState.converged.label, "converged", "the quiet case")
        expect(ConvergenceState.pending(unacked: 74, stalled: false).label, "pending 74",
               "the incident's count, named")
        expectTrue(ConvergenceState.pending(unacked: 74, stalled: true).needsAttention,
                   "past the threshold, a human is wanted")
        expectTrue(ConvergenceState.blockedLocalOnly(unacked: 74, reason: "refused")
                       .needsAttention,
                   "blocked always wants a human")
        expect(ConvergenceState.recovering(phase: .replayed, unacked: 3).unacked, 3,
               "a recovering room still reports what is local-only")
        expectTrue(!ConvergenceState.pending(unacked: 1, stalled: false).isConverged,
                   "pending is never converged")
    }

    /// gh#440: a phone-side model/reasoning edit serializes the whole config
    /// value. The board-owned push tuple must survive that Codable round trip
    /// even though no iOS picker owns or changes it.
    private static func githubPushTupleRoundTrips() {
        let contract = GithubPushContract(contentsWrite: true, workflowsWrite: true)
        let config = ChatConfig(harness: "codex", model: "gpt-5.6-terra",
                                reasoning: "high", sandbox: "workspace-write",
                                pushRepo: "owner/widget", pushContract: contract)
        do {
            let encoded = try JSONEncoder().encode(config)
            let decoded = try JSONDecoder().decode(ChatConfig.self, from: encoded)
            expect(decoded.pushRepo, config.pushRepo, "iOS preserves pushRepo")
            expect(decoded.pushContract, config.pushContract, "iOS preserves pushContract")
        } catch {
            expectTrue(false, "iOS ChatConfig push tuple round trip: \(error)")
        }
    }
}

/// A convergence driver on a throwaway directory — the spec must never write
/// into the real device's outbox.
@MainActor
private func specRecovery(docId: String) -> ConvergenceRecovery {
    let root = URL(fileURLWithPath: NSTemporaryDirectory())
        .appendingPathComponent("convergence-spec-\(UUID().uuidString)", isDirectory: true)
    return ConvergenceRecovery(docId: docId, journal: FileConvergenceJournal(root: root))
}

/// Chat b5b3c796's shape: a room holding `shared` entries, and a phone holding
/// those plus `offline` entries it committed locally.
@MainActor
private func incidentFixture(shared: Int, offline: Int) -> (LoroDoc, LoroDoc, [String]) {
    let cloud = LoroDoc()
    try? cloud.setPeerId(peer: 1)
    var expected: [String] = []
    for index in 0..<shared {
        let id = String(format: "shared-%04d", index)
        appendSyncSpecEntry(to: cloud, id: id, deviceId: "phone-spec", index: index)
        expected.append(id)
    }
    let laptop = LoroDoc()
    if let bytes = try? cloud.export(mode: .snapshot) {
        _ = try? laptop.importWith(bytes: bytes, origin: "spec")
    }
    try? laptop.setPeerId(peer: 2)
    for index in 0..<offline {
        let id = String(format: "offline-%04d", index)
        appendSyncSpecEntry(to: laptop, id: id, deviceId: "phone-spec", index: shared + index)
        expected.append(id)
    }
    return (cloud, laptop, expected)
}

/// One transcript entry in the doc schema `schema.rs` writes — text bodies as
/// LoroText containers, never LWW value rewrites.
@MainActor
private func appendSyncSpecEntry(to doc: LoroDoc, id: String, deviceId: String, index: Int) {
    do {
        let map = try doc.getList(id: "messages").pushContainer(child: LoroMap())
        try map.insert(key: "id", v: id)
        try map.insert(key: "role", v: index % 2 == 0 ? "user" : "assistant")
        try map.insert(key: "createdAt", v: Int64(1_700_000_000_000 + index))
        try map.insert(key: "deviceId", v: deviceId)
        try map.insert(key: "status", v: "complete")
        let parts = try map.insertContainer(key: "parts", child: LoroList())
        let part = try parts.pushContainer(child: LoroMap())
        try part.insert(key: "id", v: "\(id)-p0")
        try part.insert(key: "kind", v: "text")
        let text = try part.insertContainer(key: "text", child: LoroText())
        try text.insert(pos: 0, s: "entry \(index) body")
        doc.commit()
    } catch {
        preconditionFailure("could not construct spec entry: \(error)")
    }
}

private func appendSyncSpecCommand(to doc: LoroDoc, id: String, deviceId: String) {
    do {
        let map = try doc.getList(id: "commands").pushContainer(child: LoroMap())
        try map.insert(key: "id", v: id)
        try map.insert(key: "kind", v: "interrupt")
        try map.insert(key: "payload", v: LoroValue.map(value: ["kind": .string(value: "interrupt")]))
        try map.insert(key: "issuedBy", v: deviceId)
        try map.insert(key: "issuedAt", v: 1)
        try map.insert(key: "status", v: "pending")
        doc.commit()
    } catch {
        preconditionFailure("could not construct boundary command: \(error)")
    }
}
