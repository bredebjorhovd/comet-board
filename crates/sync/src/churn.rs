//! Room churn — sessions that die young, counted (gh#527).
//!
//! THE INCIDENT: on the evening of 2026-08-19 the engine's edge-connection
//! census reported "10 of 10 live" and "14 of 14 live" all evening while every
//! session room in the fleet was in a dial/die/redial loop and the phone sat
//! on 22 rooms' worth of maxed-out reconnect backoff. Nothing was wrong with
//! the gauge's arithmetic. It counts sockets that are joined AT THE INSTANT IT
//! IS ASKED, and in a join-then-die loop a room genuinely is joined a good
//! fraction of the time — the room answers the join, lives a second or two,
//! and dies. A point-in-time census cannot see that, and by construction never
//! will: the fault is not in any sample, it is in the sequence of them.
//!
//! So this counts the sequence. Every session end that had actually joined is
//! recorded with whether it lasted [`crate::room`]'s `HEALTHY_SESSION` — the
//! same 30s line that decides whether the backoff ladder resets — and the
//! health surfaces read a RATE over the last hour rather than a snapshot. A
//! room dying every 40 seconds is then a number, not a vibe.
//!
//! **Why this is process-global state.** The alternative — a counter on each
//! `RoomClient` — loses precisely the incident it is for: a client whose room
//! keeps dying is also a client its supervisor keeps REBUILDING
//! (`workspace_host.rs`), and every rebuild would zero the count. Churn is a
//! property of the ROOM across every client that has held it, so it is keyed
//! by room id and outlives them all. It is bounded on both axes (rooms and
//! events per room), pruned to the window on every write, and the room actor
//! is its only writer.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Window every rate here is reported over.
///
/// An hour because that is the unit an operator reasons in mid-incident — "is
/// it still happening?" — and because the edge's own free-tier duration cap
/// resets at UTC midnight, so a per-hour rate is what distinguishes a cap just
/// hit from a room that has been dying all day. Matches `CHURN_WINDOW_MS` in
/// edge/src/socket-log.ts, which counts the same deaths from the other end.
pub const CHURN_WINDOW: Duration = Duration::from_secs(60 * 60);

/// Ends retained per room. The ladder caps at one dial per 30s, so an hour of
/// the worst possible churn is ~120 — this is that with room to spare, and it
/// bounds the memory a permanently sick room can cost.
const MAX_EVENTS_PER_ROOM: usize = 256;
/// Rooms retained. An engine holds a room per open chat plus the workspace and
/// registry rooms; past this the least recently churning room is dropped,
/// because a census that could grow without bound would be a leak wearing a
/// diagnostic's clothes.
const MAX_ROOMS: usize = 512;

#[derive(Debug, Clone, Copy)]
struct SessionEnd {
    at: Instant,
    /// The session reached `JoinResponseOk` and then ended inside
    /// HEALTHY_SESSION. This is the churn signal: a room that refuses is
    /// visible as "down" already; a room that ANSWERS and dies is not.
    young: bool,
}

/// One room's churn, as a health surface reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoomChurn {
    pub room_id: String,
    /// Joined sessions that ended inside HEALTHY_SESSION in the last hour.
    pub died_young_last_hour: usize,
    /// Every joined session that ended in the last hour, healthy or not — the
    /// denominator, so "3 short of 40" and "3 short of 3" read differently.
    pub sessions_last_hour: usize,
}

/// The whole engine's churn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChurnCensus {
    /// Rooms with at least one young death in the window.
    pub rooms_churning: usize,
    pub died_young_last_hour: usize,
    pub sessions_last_hour: usize,
    /// The churning rooms, worst first. Named, because a count sends an
    /// operator looking and only a name sends them to the room.
    pub rooms: Vec<RoomChurn>,
}

impl ChurnCensus {
    /// Is anything churning right now? The predicate a health surface branches
    /// on, so callers do not each re-derive "> 0".
    pub fn churning(&self) -> bool {
        self.rooms_churning > 0
    }
}

type Registry = Mutex<HashMap<String, Vec<SessionEnd>>>;

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn prune(events: &mut Vec<SessionEnd>, now: Instant) {
    events.retain(|end| now.duration_since(end.at) < CHURN_WINDOW);
    if events.len() > MAX_EVENTS_PER_ROOM {
        let excess = events.len() - MAX_EVENTS_PER_ROOM;
        events.drain(0..excess);
    }
}

/// Record a session end. Called by the room actor for a session that reached
/// this client's redial loop — never for the first dial (that failure is
/// returned to the caller) and never for a clean shutdown.
///
/// `joined` is what makes a record worth keeping: a dial that never joined is
/// already visible as a down room, and counting it here would blur the one
/// distinction this module exists to draw.
pub fn record(room_id: &str, joined: bool, healthy: bool) {
    if !joined {
        return;
    }
    record_at(room_id, healthy, Instant::now());
}

fn record_at(room_id: &str, healthy: bool, now: Instant) {
    let mut rooms = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let events = rooms.entry(room_id.to_string()).or_default();
    events.push(SessionEnd {
        at: now,
        young: !healthy,
    });
    prune(events, now);
    if rooms.len() <= MAX_ROOMS {
        return;
    }
    // Over the cap: drop rooms whose window has emptied first, then the
    // quietest. A room with nothing in the window has nothing to report, so
    // dropping it costs no information at all.
    rooms.retain(|_, events| {
        prune(events, now);
        !events.is_empty()
    });
    while rooms.len() > MAX_ROOMS {
        let Some(quietest) = rooms
            .iter()
            .min_by_key(|(id, events)| (events.len(), (*id).clone()))
            .map(|(id, _)| id.clone())
        else {
            break;
        };
        rooms.remove(&quietest);
    }
}

/// One room's churn right now.
pub fn room(room_id: &str) -> RoomChurn {
    census_at(Instant::now())
        .rooms
        .into_iter()
        .find(|row| row.room_id == room_id)
        .unwrap_or_else(|| RoomChurn {
            room_id: room_id.to_string(),
            ..RoomChurn::default()
        })
}

/// Every room's churn right now.
pub fn census() -> ChurnCensus {
    census_at(Instant::now())
}

fn census_at(now: Instant) -> ChurnCensus {
    let mut rooms = registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut census = ChurnCensus::default();
    for (room_id, events) in rooms.iter_mut() {
        prune(events, now);
        if events.is_empty() {
            continue;
        }
        let died_young = events.iter().filter(|end| end.young).count();
        census.sessions_last_hour += events.len();
        census.died_young_last_hour += died_young;
        if died_young == 0 {
            continue;
        }
        census.rooms_churning += 1;
        census.rooms.push(RoomChurn {
            room_id: room_id.clone(),
            died_young_last_hour: died_young,
            sessions_last_hour: events.len(),
        });
    }
    // Worst first, then by name so the order is stable between two reads of an
    // unchanged fleet.
    census.rooms.sort_by(|a, b| {
        b.died_young_last_hour
            .cmp(&a.died_young_last_hour)
            .then(a.room_id.cmp(&b.room_id))
    });
    census
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One #[test], not four. The registry is a process-global map by design
    /// (see the module comment), and this crate's room tests drive real
    /// `RoomClient` sessions that record into it while these run — so the
    /// assertions here are scoped to rooms named for this test, and the fleet
    /// totals are never asserted absolutely.
    #[test]
    fn churn_rules() {
        let now = Instant::now();
        let sick = "churn-spec/sick";
        let fine = "churn-spec/fine";
        let unreachable = "churn-spec/unreachable";

        // The incident shape: a room that ANSWERS every join and dies. Its
        // socket is live a good fraction of the time, so a point-in-time
        // census reports it healthy — this is what sees it instead.
        for i in 0..7 {
            // checked_sub: `now` is only a few seconds into a test binary's
            // life on some platforms, and a panicking clock would be a very
            // silly way for a churn test to fail.
            let at = now.checked_sub(Duration::from_secs(60 * i)).unwrap_or(now);
            record_at(sick, false, at);
        }
        // A room doing its job: one long session that ended when the lid shut.
        record_at(fine, true, now);
        // A dial that never joined is a DOWN room, not a churning one — the
        // existing live-connection gauge already says so, and blurring the two
        // would cost the distinction this module exists to draw.
        record(unreachable, false, false);

        let census = census_at(now);
        assert!(census.churning());
        let row = census
            .rooms
            .iter()
            .find(|room| room.room_id == sick)
            .expect("the sick room is named, not merely counted");
        assert_eq!(row.died_young_last_hour, 7);
        assert_eq!(row.sessions_last_hour, 7);
        assert!(
            !census.rooms.iter().any(|room| room.room_id == fine),
            "a room with one long session is not churning"
        );
        assert!(
            !census.rooms.iter().any(|room| room.room_id == unreachable),
            "a room that never joined is DOWN, which is a different fact"
        );

        // A rate, not a total: yesterday's outage does not make today's fleet
        // look sick.
        let healed = now + CHURN_WINDOW + Duration::from_secs(1);
        assert!(
            !census_at(healed)
                .rooms
                .iter()
                .any(|room| room.room_id == sick),
            "the window is what makes this readable a day later"
        );

        // And a room dying forever must not GROW forever.
        let loop_room = "churn-spec/loop";
        for i in 0..(MAX_EVENTS_PER_ROOM * 3) {
            record_at(loop_room, false, now + Duration::from_millis(i as u64));
        }
        let counted = census_at(now + Duration::from_secs(1))
            .rooms
            .iter()
            .find(|room| room.room_id == loop_room)
            .map(|room| room.died_young_last_hour)
            .unwrap_or_default();
        assert!(
            counted <= MAX_EVENTS_PER_ROOM,
            "a room dying forever must not grow forever: {counted}"
        );
        assert!(counted > 0, "and must still report that it is dying");
    }
}
