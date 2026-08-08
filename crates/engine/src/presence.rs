//! What this engine believes about which devices are connected (gh#145).
//!
//! Presence used to be a timestamp race: every engine wrote `presence/{deviceId}
//! → now` into both sync rooms every 15s, and a reader called a device online if
//! its newest beat was younger than 45s. That reading is only correct while the
//! beats keep coming — and the beats are exactly what stopped the edge's Durable
//! Objects from ever hibernating. A `%EPH` frame is a real message, so an idle
//! fleet kept two objects awake around the clock: 10,800 GB-s/day each, 83% of
//! the daily free tier, before a single agent ran.
//!
//! So the beat is gone, and with it the assumption that silence means death.
//! Presence is now a BELIEF this engine holds, moved only by evidence:
//!
//! - **The room's answer.** `SessionRoom` derives `{deviceId: lastSeenAt}` from
//!   its own socket set (auto-pong timestamps, which the runtime stamps while
//!   hibernating, for free) and pushes it on every join and every close. Those
//!   are events that already woke the object, so the channel is free — and it
//!   makes a device arriving or cleanly leaving instant. It is also STICKY: a
//!   quiet room says nothing, and saying nothing must never be read as "gone",
//!   which was the whole of gh#126.
//! - **The relay probe.** `GET /device/{id}/status` asks the device's own
//!   `DeviceRoom` whether its host socket is live. That room shares no machinery
//!   with the sync rooms, has always derived liveness from auto-pongs, and costs
//!   ~2ms of duration per ask — it is the cheap channel, so it is the one that
//!   gets polled. It is also the ONLY thing that can catch a socket whose uplink
//!   died silently: the runtime never closes such a socket, so the sync room
//!   keeps listing a corpse until somebody makes it look again.
//!
//! A device is online when either path says so. It goes offline when the room
//! drops it from its answer, or when the relay has failed to find its host
//! [`RELAY_MISS_STRIKES`] times running — one negative is a reconnect, two is an
//! absence. A probe that errors is not evidence of anything and moves nothing:
//! this engine's own broken uplink must never be published as someone else's
//! outage.

use std::collections::{HashMap, HashSet};

use comet_doc::presence_key;
use comet_sync::RoomClient;

/// The device ids a room's latest `%EPH` answer lists.
///
/// The wire shape is unchanged across gh#145 (`presence/{deviceId}` → ms), so an
/// engine that predates the change still reads a room that now derives its own
/// answer — and this reads the entries a legacy engine still beats in, too. Only
/// the KEYS matter here: the room writes an entry for exactly the devices it can
/// see and deletes the rest, so presence is membership, not freshness. Freshness
/// was the beat, and the beat is what never let the room hibernate.
pub fn room_presence_ids(client: &RoomClient) -> HashSet<String> {
    let prefix = presence_key("");
    client
        .ephemeral()
        .get_all_states()
        .into_keys()
        .filter_map(|key| key.strip_prefix(&prefix).map(str::to_string))
        .filter(|id| !id.is_empty())
        .collect()
}

/// How long a relay confirmation stands on its own, once the rooms have stopped
/// mentioning the device. Wider than the probe cadence so an ordinary probe
/// keeps a device continuously confirmed, and narrower than the UI's 70s window
/// so a confirmation never outlives the badge it justifies.
pub const RELAY_FRESH_MS: i64 = 45_000;

/// Consecutive "no live host" answers before the relay is believed over a room
/// that is still listing the device.
///
/// One is not enough: a host is briefly hostless across every edge deploy and
/// every supersede-on-reconnect, and a single unlucky probe there would flash
/// the whole fleet offline. Two consecutive answers, at the probe cadence, is
/// a device that has been unreachable for a minute — which is what "offline"
/// is supposed to mean.
pub const RELAY_MISS_STRIKES: u32 = 2;

/// Which sync room an answer came from. They see different socket sets — a
/// teammate's laptop only ever appears on the org registry — so each room may
/// only ever retract devices IT could have seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceRoom {
    /// `ws3/{orgId}/{userId}` — this user's own fleet.
    Workspace,
    /// `orgdev1/{orgId}` — every device in the org.
    Org,
}

/// This engine's belief about who is connected. Pure and clock-injected, so the
/// rules are testable without a room, an edge or a wall clock.
#[derive(Debug, Default, Clone)]
pub struct PresenceBelief {
    /// Devices the workspace room's most recent answer listed.
    workspace: HashSet<String>,
    /// Devices the org registry's most recent answer listed.
    org: HashSet<String>,
    /// Local ms of the last relay probe that found the device's host socket up.
    relay: HashMap<String, i64>,
    /// Consecutive relay probes that found no host socket.
    misses: HashMap<String, u32>,
}

impl PresenceBelief {
    /// Replace a room's answer wholesale. The room is authoritative about its
    /// OWN socket set, so a device it no longer lists is one it no longer sees —
    /// and a device it lists is alive enough to clear the relay's strikes.
    pub fn room_answered(&mut self, room: PresenceRoom, devices: HashSet<String>) {
        for device in &devices {
            self.misses.remove(device);
        }
        match room {
            PresenceRoom::Workspace => self.workspace = devices,
            PresenceRoom::Org => self.org = devices,
        }
    }

    /// The relay found this device's host socket live.
    pub fn relay_alive(&mut self, device_id: &str, now: i64) {
        self.relay.insert(device_id.to_string(), now);
        self.misses.remove(device_id);
    }

    /// The relay answered, and there was no live host. Counted rather than
    /// acted on — see [`RELAY_MISS_STRIKES`].
    pub fn relay_absent(&mut self, device_id: &str) {
        *self.misses.entry(device_id.to_string()).or_insert(0) += 1;
    }

    /// Is this device connected, as far as this engine can tell?
    pub fn online(&self, device_id: &str, now: i64) -> bool {
        // An independent path has failed to reach the device twice running.
        // That outranks a room still listing it, because a room listing a
        // device is exactly what a silently-dead socket looks like.
        if self.misses.get(device_id).copied().unwrap_or(0) >= RELAY_MISS_STRIKES {
            return false;
        }
        if self.workspace.contains(device_id) || self.org.contains(device_id) {
            return true;
        }
        self.relay
            .get(device_id)
            .is_some_and(|at| now.saturating_sub(*at) < RELAY_FRESH_MS)
    }

    /// Devices worth asking the relay about: everything either room mentions or
    /// the relay has ever confirmed. The caller adds the registry's own rows —
    /// this is only what the belief itself knows about.
    pub fn known(&self) -> HashSet<String> {
        self.workspace
            .iter()
            .chain(self.org.iter())
            .chain(self.relay.keys())
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000_000;

    fn set(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| (*id).to_string()).collect()
    }

    #[test]
    fn a_room_answer_puts_a_device_online_and_taking_it_back_takes_it_offline() {
        let mut belief = PresenceBelief::default();
        assert!(!belief.online("box", NOW), "nothing is believed by default");
        belief.room_answered(PresenceRoom::Workspace, set(&["box", "laptop"]));
        assert!(belief.online("box", NOW));
        assert!(belief.online("laptop", NOW));
        // The box shuts down: its close wakes the room, which answers again
        // without it. That is the whole offline path for a clean departure.
        belief.room_answered(PresenceRoom::Workspace, set(&["laptop"]));
        assert!(!belief.online("box", NOW));
        assert!(belief.online("laptop", NOW));
    }

    /// The gh#126 rule, restated for a world with no beat: silence is not
    /// evidence. A room that has said nothing for hours — because nothing has
    /// happened in it, which is now the normal state of an idle night — still
    /// believes what it was last told.
    #[test]
    fn a_quiet_room_keeps_its_answer_however_old() {
        let mut belief = PresenceBelief::default();
        belief.room_answered(PresenceRoom::Workspace, set(&["box"]));
        assert!(belief.online("box", NOW + 8 * 3_600_000));
    }

    /// The two rooms see different fleets: a teammate's laptop is only ever on
    /// the org registry. A workspace answer that omits it says nothing about it.
    #[test]
    fn one_rooms_answer_never_retracts_a_device_it_cannot_see() {
        let mut belief = PresenceBelief::default();
        belief.room_answered(PresenceRoom::Org, set(&["teammate-laptop"]));
        belief.room_answered(PresenceRoom::Workspace, set(&["my-laptop"]));
        assert!(belief.online("teammate-laptop", NOW));
        assert!(belief.online("my-laptop", NOW));
    }

    /// The case the beat used to cover and the socket set cannot: a laptop lid
    /// closes, the runtime never fires a close, and the room goes on listing a
    /// corpse. Only the independent path can break that.
    #[test]
    fn two_relay_misses_overrule_a_room_still_listing_a_corpse() {
        let mut belief = PresenceBelief::default();
        belief.room_answered(PresenceRoom::Workspace, set(&["box"]));
        belief.relay_absent("box");
        assert!(
            belief.online("box", NOW),
            "one miss is a reconnect, not an outage"
        );
        belief.relay_absent("box");
        assert!(!belief.online("box", NOW));
    }

    #[test]
    fn the_box_coming_back_clears_its_strikes() {
        let mut belief = PresenceBelief::default();
        belief.room_answered(PresenceRoom::Workspace, set(&["box"]));
        belief.relay_absent("box");
        belief.relay_absent("box");
        assert!(!belief.online("box", NOW));
        // Either path is enough to rehabilitate it.
        belief.relay_alive("box", NOW);
        assert!(belief.online("box", NOW));
        belief.relay_absent("box");
        belief.relay_absent("box");
        assert!(!belief.online("box", NOW));
        belief.room_answered(PresenceRoom::Workspace, set(&["box"]));
        assert!(belief.online("box", NOW));
    }

    /// A relay confirmation stands on its own — this is what keeps a device
    /// online when our own sync rooms are down and cannot answer at all.
    #[test]
    fn a_relay_confirmation_alone_reads_online_until_it_ages_out() {
        let mut belief = PresenceBelief::default();
        belief.relay_alive("box", NOW);
        assert!(belief.online("box", NOW));
        assert!(belief.online("box", NOW + RELAY_FRESH_MS - 1));
        assert!(!belief.online("box", NOW + RELAY_FRESH_MS));
    }

    #[test]
    fn known_devices_are_everything_either_path_has_mentioned() {
        let mut belief = PresenceBelief::default();
        belief.room_answered(PresenceRoom::Workspace, set(&["mine"]));
        belief.room_answered(PresenceRoom::Org, set(&["theirs"]));
        belief.relay_alive("probed", NOW);
        assert_eq!(belief.known(), set(&["mine", "theirs", "probed"]));
    }
}
