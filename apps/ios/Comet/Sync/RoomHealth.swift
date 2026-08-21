// What the phone knows about its own rooms, said out loud (gh#527).
//
// THE INCIDENT: 2026-08-19, ~00:30. Sends left the phone, the box's journal
// logged `nudge: chat doc opened` for every one of them, and nothing ever came
// back. The reply rides the session room; the session rooms were dying 1006
// mid-life; every room's ladder climbed to its 30s cap and stayed there. The
// app knew all of this — `RoomClient` logged "redialing in 30000ms" for 22
// rooms — and showed a transcript that looked merely quiet. A person cannot
// tell "nobody has answered yet" from "nothing you type will arrive for as
// long as this lasts", and the app was the only thing in the system that could
// have told them.
//
// So the count that existed internally becomes a fact on screen. Two rules
// hold it honest:
//
//   1. **A room mid-redial is not degraded.** A dropped socket is normal — the
//      lid closed, the train went into a tunnel, the edge deployed — and a
//      banner that fires on every reconnect is a banner people learn to
//      ignore. Degraded means the ladder has CLIMBED (`degradedRungMs`): the
//      room has failed repeatedly, which takes seconds, not milliseconds.
//   2. **Churn is counted the way every other side counts it.** A session that
//      joined and died inside `ReconnectBackoff.healthySessionNs` is the
//      join-then-die shape — the same 30s line `crates/sync/src/churn.rs` and
//      `edge/src/socket-log.ts` draw. It is the difference between "the edge
//      is unreachable" and "the edge answers and then dies", and the second one
//      is what happened.
//
// The diagnostics text is a pure function of the snapshot on purpose. During
// the incident the operator could not get anything off the app at all —
// diagnosis ran on paraphrases over SSH — so what the banner knows has to be
// copyable and shareable, and what is copyable has to be assertable without a
// simulator (`SyncSpecRunner`, and the text scan in crates/sync/tests/ios_room.rs).

import Foundation
import Observation

/// One room's last known redial state.
struct RoomRedialState: Equatable {
    var roomId: String
    /// The rung the next wait is drawn from. `ReconnectBackoff.baseMs` means
    /// the room has not failed yet.
    var rungMs: Int
    /// The last session reached `JoinResponseOk`.
    var joined: Bool
    /// The last session lasted long enough to prove the room works.
    var healthy: Bool
    /// When this room last went into a redial.
    var since: Date
}

/// A moment's reading of every room this app holds — the value the banner and
/// the diagnostics text are both derived from, so the two cannot disagree.
struct RoomHealthSnapshot: Equatable {
    /// Rooms the app has opened.
    var known: Int = 0
    /// Rooms currently waiting out a redial.
    var reconnecting: Int = 0
    /// Rooms whose ladder has climbed past the base rung — repeated failure,
    /// not one dropped socket.
    var laddered: Int = 0
    /// Sessions across all rooms that JOINED and died inside 30s in the last
    /// hour: the edge answering and then dying, rather than refusing.
    var diedYoungLastHour: Int = 0
    /// The worst rung any room is sitting on.
    var worstRungMs: Int = 0
    /// The rooms on the ladder, worst first — named, because a count sends a
    /// person looking and only a name says which chat is affected.
    var ladderedRooms: [String] = []

    /// Is this app in the state gh#527 went silent in?
    ///
    /// Deliberately NOT "any room is reconnecting": that is ordinary life on a
    /// phone. It is "at least one room has failed repeatedly", which is the
    /// state in which what you type does not arrive and nothing else on screen
    /// says so.
    var degraded: Bool { laddered > 0 }

    /// The one line the transcript shows. Says the consequence first — a
    /// person needs to know their reply is delayed before they need to know
    /// how many rooms are involved.
    var headline: String {
        guard degraded else { return "" }
        let scope = known >= laddered && known > 0
            ? "\(laddered) of \(known) rooms"
            : (laddered == 1 ? "1 room" : "\(laddered) rooms")
        return "Rooms reconnecting — replies delayed (\(scope))"
    }

    /// The sentence under it: WHICH failure this is, since the two look
    /// identical from a dead transcript and are fixed in different places.
    var detail: String {
        guard degraded else { return "" }
        if diedYoungLastHour > 0 {
            return "The edge is answering and then dropping the connection "
                + "(\(diedYoungLastHour) short session(s) in the last hour). "
                + "Sends are saved and will go when a room stays up."
        }
        return "The edge is not answering. Sends are saved and will go when a room comes back."
    }
}

/// Every room's redial state, in one place, on the main actor.
///
/// A singleton because the fact it reports is fleet-wide — "22 of 22" is not
/// something any one room can say — and because every `RoomClient` in the app
/// has to reach it without being handed a reference through four layers of
/// store construction it does not otherwise need.
@MainActor
@Observable
final class RoomHealth {
    static let shared = RoomHealth()

    /// The rung at which a room counts as degraded rather than merely
    /// reconnecting.
    ///
    /// Three consecutive failures (250 → 500 → 1000 → 2000), which is about
    /// four seconds. One dropped socket is ordinary life on a phone — a lid, a
    /// tunnel, an edge deploy — and a banner that fires on it is a banner
    /// people learn to scroll past. Three in a row is a room that is not
    /// coming back on its own, which is worth interrupting someone for.
    static let degradedRungMs = ReconnectBackoff.baseMs * 8

    /// How long a young death stays in the churn count — the window
    /// `comet_sync::churn::CHURN_WINDOW` and `edge/src/socket-log.ts` use.
    static let churnWindow: TimeInterval = 60 * 60

    private(set) var snapshot = RoomHealthSnapshot()

    private var rooms: [String: RoomRedialState] = [:]
    /// Young deaths, newest last. Bounded by the window and by count, because
    /// a permanently sick fleet must not grow a list forever.
    private var youngDeaths: [Date] = []
    private static let maxYoungDeaths = 512

    /// A room joined and is holding. Clears its ladder state.
    func joined(room roomId: String) {
        rooms[roomId] = nil
        recompute()
    }

    /// A room is opening — known, but not yet judged either way. This is what
    /// makes the denominator honest: "8 of 22" needs the 22.
    func opening(room roomId: String) {
        known.insert(roomId)
        recompute()
    }

    /// A room went into a redial. `joined`/`healthy` are the same pair the
    /// backoff decision is made on, so the banner and the ladder cannot
    /// disagree about what happened.
    func redialing(room roomId: String, rungMs: Int, joined: Bool, healthy: Bool,
                   now: Date = Date()) {
        known.insert(roomId)
        rooms[roomId] = RoomRedialState(roomId: roomId, rungMs: rungMs, joined: joined,
                                        healthy: healthy, since: now)
        if joined && !healthy {
            youngDeaths.append(now)
            if youngDeaths.count > Self.maxYoungDeaths {
                youngDeaths.removeFirst(youngDeaths.count - Self.maxYoungDeaths)
            }
        }
        recompute(now: now)
    }

    /// A room was closed by the app (the chat was released, or sign-out).
    func forget(room roomId: String) {
        rooms[roomId] = nil
        known.remove(roomId)
        recompute()
    }

    private var known: Set<String> = []

    private func recompute(now: Date = Date()) {
        youngDeaths.removeAll { now.timeIntervalSince($0) > Self.churnWindow }
        let threshold: Int = Self.degradedRungMs
        let open: [RoomRedialState] = Array(rooms.values)
        var laddered: [RoomRedialState] = open.filter { room in room.rungMs >= threshold }
        // Worst rung first, then by name, so two reads of an unchanged fleet
        // produce the same list.
        laddered.sort { left, right in
            if left.rungMs == right.rungMs { return left.roomId < right.roomId }
            return left.rungMs > right.rungMs
        }
        let rungs: [Int] = open.map { room in room.rungMs }
        var next = RoomHealthSnapshot()
        next.known = known.count
        next.reconnecting = rooms.count
        next.laddered = laddered.count
        next.diedYoungLastHour = youngDeaths.count
        next.worstRungMs = rungs.max() ?? 0
        next.ladderedRooms = laddered.map(\.roomId)
        snapshot = next
    }

    /// Everything this app knows about the state, as text a person can paste
    /// into an issue or send to whoever is holding the edge.
    ///
    /// Built from the snapshot alone so it says exactly what the banner says
    /// and nothing it cannot support. Tonight's version of this was "the
    /// operator could not even screenshot the app".
    func diagnostics(now: Date = Date()) -> String {
        RoomHealth.diagnosticsText(snapshot: snapshot, at: now)
    }

    /// Pure, so the spec runner can assert the report without a live edge.
    static func diagnosticsText(snapshot: RoomHealthSnapshot, at now: Date) -> String {
        var lines: [String] = []
        lines.append("Comet sync diagnostics — \(ISO8601DateFormatter().string(from: now))")
        lines.append(snapshot.degraded ? snapshot.headline : "Rooms: holding")
        if snapshot.degraded { lines.append(snapshot.detail) }
        lines.append("rooms known: \(snapshot.known)")
        lines.append("rooms reconnecting: \(snapshot.reconnecting)")
        lines.append("rooms on the backoff ladder: \(snapshot.laddered)")
        lines.append("worst rung: \(snapshot.worstRungMs)ms (cap \(ReconnectBackoff.capMs)ms)")
        lines.append("sessions that joined and died inside 30s, last hour: \(snapshot.diedYoungLastHour)")
        if !snapshot.ladderedRooms.isEmpty {
            // Named, and bounded: a paste of 22 room ids is still readable, a
            // paste of 400 is not.
            let named = snapshot.ladderedRooms.prefix(12).joined(separator: ", ")
            let more = snapshot.ladderedRooms.count - min(12, snapshot.ladderedRooms.count)
            lines.append("rooms: \(named)" + (more > 0 ? " (+\(more) more)" : ""))
        }
        return lines.joined(separator: "\n")
    }
}
