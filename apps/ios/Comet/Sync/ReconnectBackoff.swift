// The room's redial schedule — port of the ladder in `crates/sync/src/room.rs`
// and the spread in `crates/sync/src/jitter.rs` (gh#396, ported here by
// gh#405).
//
// It is a value type rather than three fields on `RoomClient` for one reason:
// the decision is the thing that was wrong, and a decision welded to a
// `URLSessionWebSocketTask` can only be checked against a live edge. Here it is
// arithmetic, and `SyncSpecRunner` (`-sync-spec`) asserts it.
//
// Two rules, both of which only matter when the edge is DEGRADED rather than
// down — which is the state this account's edge is usually in when it is not
// well, since the Durable Objects free-tier duration cap kills sessions
// mid-life rather than refusing them:
//
//   1. Only a session that lasted resets the ladder. Reaching JoinResponseOk
//      is not evidence the room works; a DO that accepts the join and then
//      dies answers the join every single time.
//   2. Every wait is jittered. The rung is a ceiling, not a schedule: rooms
//      that failed together (one uplink, one edge deploy, one return to the
//      foreground) must not redial together forever.

import Foundation

struct ReconnectBackoff {
    /// Constants mirrored from room.rs — held to it by
    /// `crates/sync/tests/ios_room.rs`, which reads both files as text and
    /// fails in CI when either side moves. gh#405 is what happens without
    /// that: room.rs changed these rules and this file did not hear about it
    /// for a release.
    static let baseMs = 250
    static let capMs = 30_000

    /// How long a session must stay joined for its end to count as a working
    /// session and earn a fresh ladder (room.rs `HEALTHY_SESSION`).
    ///
    /// Reaching `JoinResponseOk` is NOT that proof, and treating it as proof is
    /// the defect: a room whose DO accepts the join and then dies — duration
    /// cap hit mid-session, or the `ctx.abort()` of the WASM-poisoning
    /// escalation (gh#378) — answers the join on every dial, so a
    /// reset-on-join redials at `baseMs` forever. Four dials a second, per
    /// room, with no ceiling, from a battery-powered device, against an edge
    /// that is already unwell. Requiring a lifetime instead bounds a
    /// join-then-die loop to roughly one dial per session and lets the ladder
    /// climb to `capMs` when the deaths stay fast.
    static let healthySessionNs: UInt64 = 30_000_000_000

    /// The rung the next wait is drawn from — a ceiling, not the wait itself.
    private(set) var rungMs = ReconnectBackoff.baseMs

    /// A uniform draw in `[0, span)`.
    ///
    /// `Int.random` because Swift ships one. The Rust half hand-rolls the same
    /// idea out of the nanosecond clock (`jitter.rs`) only because it refuses a
    /// `rand` dependency in the sync crate — nothing about the schedule needs
    /// that trick here, and nothing here needs unpredictability either: this is
    /// herd-breaking, not a nonce.
    static func spread(_ spanMs: Int) -> Int {
        spanMs > 0 ? Int.random(in: 0..<spanMs) : 0
    }

    /// The wait before the next dial, advancing the ladder by one rung.
    ///
    /// `healthy` is applied first, so a session worth a reset gets one whatever
    /// the rung had climbed to. The wait itself is a jittered
    /// `[rung/2, rung)` — half a step of spread at every rung, widening with
    /// the rung, which is where the rooms are most piled up.
    ///
    /// `spread` is a parameter so the spec runner can pin the draw; production
    /// never passes it.
    mutating func nextDelayMs(healthy: Bool,
                              spread: (Int) -> Int = ReconnectBackoff.spread) -> Int {
        if healthy { rungMs = ReconnectBackoff.baseMs }
        let half = rungMs / 2
        let delay = half + spread(half)
        rungMs = min(rungMs * 2, ReconnectBackoff.capMs)
        return delay
    }

    /// Whether a session that was joined at `joinedAt` and ended `now` lasted
    /// long enough to be evidence the room works. A session that never joined
    /// (`nil`) never is.
    static func isHealthy(joinedAt: DispatchTime?, now: DispatchTime = .now()) -> Bool {
        guard let joinedAt else { return false }
        // Monotonic, so `now` cannot precede the join — but the subtraction is
        // unsigned, and an underflow here would read as a healthy session.
        guard now.uptimeNanoseconds >= joinedAt.uptimeNanoseconds else { return false }
        return now.uptimeNanoseconds - joinedAt.uptimeNanoseconds >= healthySessionNs
    }
}
