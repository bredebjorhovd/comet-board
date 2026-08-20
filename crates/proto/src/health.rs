//! Edge connection health — what an engine actually holds open, as opposed to
//! what it believes about itself (gh#116).
//!
//! The incident this exists for: an edge redeploy cycled the Durable Objects at
//! 10:06, every one of the box's edge sockets died, and the engine ran on for
//! 25 minutes looking perfectly healthy. Locally it was healthy —
//! `comet-board list` answered, dispatches ran. Remotely it did not exist: the
//! iOS host sweep correctly reported that nobody hosts a board. The state was
//! visible only by grepping journald for one WARN line.
//!
//! So this is the answer to a question nothing could ask before: *which edge
//! connections does this engine hold right now?* It is deliberately a count of
//! live sockets rather than a boolean, because the useful distinction is not
//! online/offline but "some rooms are down" versus [`EdgeHealth::dark`] — up,
//! signed in, and holding nothing.

use serde::{Deserialize, Serialize};

/// A point-in-time census of one engine's edge connections.
///
/// The three named connections are `Option<bool>`: `None` means "not
/// configured on this engine" (local mode, or no relay), which is a different
/// thing from `Some(false)` — "configured, and down".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EdgeHealth {
    /// The edge this engine is WIRED to; `None` means it is not syncing at all
    /// — local mode, or signed out at assembly. Either way nothing here can be
    /// wrong, which is why [`EdgeHealth::dark`] needs it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge_url: Option<String>,
    /// The DeviceRoom host socket — how any remote viewer reaches this device
    /// at all. Down means invisible, whatever else is up.
    #[serde(default)]
    pub host_relay: Option<bool>,
    /// `ws4/{orgId}/{userId}` — this user's private workspace room (chats,
    /// spaces, sessions, presence).
    #[serde(default)]
    pub workspace_room: Option<bool>,
    /// The workspace room's `%EPH` presence sub-room (gh#126). Joined
    /// separately from the doc room on the same socket, so `Some(true)` for
    /// [`Self::workspace_room`] with `Some(false)` here is a real state — doc
    /// sync fine, every heartbeat silently dropped — and the one the census
    /// exists to name.
    #[serde(default)]
    pub workspace_presence: Option<bool>,
    /// `orgdev1/{orgId}` — the org-wide device registry, the index a teammate
    /// needs before they can address this box.
    #[serde(default)]
    pub org_registry: Option<bool>,
    /// The org registry room's `%EPH` sub-room — the channel a teammate's
    /// online dot actually rides (gh#126).
    #[serde(default)]
    pub org_presence: Option<bool>,
    /// Chat docs open on this engine, and how many hold a live session room.
    #[serde(default)]
    pub chat_rooms_open: usize,
    #[serde(default)]
    pub chat_rooms_live: usize,
    /// Chat rooms holding content the edge has not acknowledged (gh#483).
    ///
    /// A LIVE room can be in here — that is the whole point. gh#483's incident
    /// was a Mac whose session room was joined, ponging and presence-live while
    /// the cloud sat 74 transcript entries behind it, permanently. Socket
    /// liveness is [`Self::chat_rooms_live`]; whether what was typed here
    /// exists anywhere else is this.
    #[serde(default)]
    pub chat_rooms_unconverged: usize,
    /// Chat rooms mid shallow-history recovery right now.
    #[serde(default)]
    pub chat_rooms_recovering: usize,
    /// Chat rooms the edge refuses content from: live, and local-only.
    #[serde(default)]
    pub chat_rooms_blocked: usize,
    /// Semantic entries (transcript + ledger) across all open chats that exist
    /// only on this device.
    #[serde(default)]
    pub unacknowledged_entries: usize,
    /// The rooms behind those counts, named. A count tells an operator that
    /// something is stuck; only the name tells them which chat to open — the
    /// same reason [`Self::summary`] names down connections instead of
    /// counting them. Converged rooms are omitted, so this is empty in the
    /// ordinary case and short in the bad one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unconverged_rooms: Vec<RoomConvergence>,
}

/// One room that is not converged, as the census sees it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomConvergence {
    pub chat_id: String,
    /// `converged` / `pending N` / `recovering (phase, N local-only)` /
    /// `blocked/local-only (…)` — the room's own words
    /// (`comet_sync::ConvergenceState::label`), not a re-derivation.
    pub state: String,
    /// Semantic entries in this room that exist only on this device.
    pub unacknowledged_entries: usize,
}

impl EdgeHealth {
    /// Live connections right now.
    pub fn live(&self) -> usize {
        [self.host_relay, self.workspace_room, self.org_registry]
            .iter()
            .filter(|state| **state == Some(true))
            .count()
            + self.chat_rooms_live
    }

    /// Connections this engine is configured to hold.
    pub fn expected(&self) -> usize {
        [self.host_relay, self.workspace_room, self.org_registry]
            .iter()
            .filter(|state| state.is_some())
            .count()
            + self.chat_rooms_open
    }

    /// Is the edge configured at all?
    pub fn online_mode(&self) -> bool {
        self.edge_url.is_some()
    }

    /// The state gh#116 went dark in: the engine is configured for an edge and
    /// holds not one live connection to it. Not the same as "offline" — an
    /// engine that knows it is offline is fine; this one does not.
    pub fn dark(&self) -> bool {
        self.online_mode() && self.expected() > 0 && self.live() == 0
    }

    /// The state gh#483 went quiet in: sockets up, content stuck. Every surface
    /// that renders "synced" from [`Self::live`] alone has to ask this too —
    /// a room can be live for hours while the transcript on it goes nowhere.
    pub fn live_but_unconverged(&self) -> bool {
        self.live() > 0 && (self.chat_rooms_unconverged > 0 || self.chat_rooms_blocked > 0)
    }

    /// One line for `comet status` and `comet-board doctor`. Says what is
    /// held, and names what is missing rather than only counting it — "0 of 3"
    /// does not tell an operator which socket to go and look at.
    pub fn summary(&self) -> String {
        let Some(url) = &self.edge_url else {
            return "not syncing — local mode, or signed out when the engine started".into();
        };
        if self.expected() == 0 {
            return format!("{url}: signed out — no connections attempted");
        }
        let mut down: Vec<&str> = Vec::new();
        if self.host_relay == Some(false) {
            down.push("device room");
        }
        if self.workspace_room == Some(false) {
            down.push("workspace room");
        }
        if self.org_registry == Some(false) {
            down.push("org registry");
        }
        let chats_down = self.chat_rooms_open.saturating_sub(self.chat_rooms_live);
        let mut detail = format!("{} of {} live", self.live(), self.expected());
        if !down.is_empty() {
            detail.push_str(&format!(" — {} down", down.join(", ")));
        }
        if chats_down > 0 {
            detail.push_str(&format!(
                "{} {chats_down} of {} chat room(s) down",
                if down.is_empty() { " —" } else { "," },
                self.chat_rooms_open
            ));
        }
        // Presence dead on a doc-live room (gh#126): sync looks perfect while
        // every heartbeat is dropped — the exact state that read as a healthy
        // box rendering "offline" on every other device. Named, not counted.
        let mut presence_down: Vec<&str> = Vec::new();
        if self.workspace_room == Some(true) && self.workspace_presence == Some(false) {
            presence_down.push("workspace room");
        }
        if self.org_registry == Some(true) && self.org_presence == Some(false) {
            presence_down.push("org registry");
        }
        if !presence_down.is_empty() {
            detail.push_str(&format!(
                " — presence dead on {} (doc sync is up; this device will read \
                 offline elsewhere)",
                presence_down.join(", ")
            ));
        }
        // Content, not sockets (gh#483). Named after the socket clauses on
        // purpose: an operator reading "12 of 12 live" has to see, in the same
        // line, that one of those live rooms is holding 74 entries the edge has
        // never taken. A live room is not a converged one.
        if self.chat_rooms_blocked > 0 {
            detail.push_str(&format!(
                " — {} chat room(s) BLOCKED/local-only: the edge refuses this device's \
                 history and {} entr(ies) exist nowhere else",
                self.chat_rooms_blocked, self.unacknowledged_entries
            ));
        } else if self.chat_rooms_unconverged > 0 {
            detail.push_str(&format!(
                " — {} chat room(s) not converged: {} local entr(ies) unacknowledged \
                 by the edge",
                self.chat_rooms_unconverged, self.unacknowledged_entries
            ));
        }
        if self.chat_rooms_recovering > 0 {
            detail.push_str(&format!(
                ", {} recovering from a shallow-history cut",
                self.chat_rooms_recovering
            ));
        }
        // Name them, up to a line's worth. "1 chat room not converged" sends an
        // operator looking; "chat abc123: pending 74" sends them to the chat.
        if !self.unconverged_rooms.is_empty() {
            let named: Vec<String> = self
                .unconverged_rooms
                .iter()
                .take(3)
                .map(|room| format!("{}: {}", room.chat_id, room.state))
                .collect();
            detail.push_str(&format!(" [{}", named.join("; ")));
            if self.unconverged_rooms.len() > named.len() {
                detail.push_str(&format!(
                    "; +{} more",
                    self.unconverged_rooms.len() - named.len()
                ));
            }
            detail.push(']');
        }
        if self.dark() {
            detail.push_str(
                " — this engine believes it is online but holds NO edge connections; \
                 remote viewers cannot see it",
            );
        }
        format!("{url}: {detail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn online() -> EdgeHealth {
        EdgeHealth {
            edge_url: Some("https://edge.example".into()),
            host_relay: Some(true),
            workspace_room: Some(true),
            workspace_presence: Some(true),
            org_registry: Some(true),
            org_presence: Some(true),
            chat_rooms_open: 2,
            chat_rooms_live: 2,
            ..EdgeHealth::default()
        }
    }

    #[test]
    fn a_healthy_engine_is_not_dark() {
        let health = online();
        assert_eq!(health.live(), 5);
        assert_eq!(health.expected(), 5);
        assert!(!health.dark());
        assert!(health.summary().contains("5 of 5 live"));
    }

    #[test]
    fn one_dead_room_names_itself_without_calling_the_engine_dark() {
        let health = EdgeHealth {
            workspace_room: Some(false),
            ..online()
        };
        assert!(!health.dark());
        let summary = health.summary();
        assert!(summary.contains("4 of 5 live"), "{summary}");
        assert!(summary.contains("workspace room down"), "{summary}");
    }

    /// The incident shape: everything configured, nothing live.
    #[test]
    fn zero_live_connections_is_dark_and_says_so() {
        let health = EdgeHealth {
            host_relay: Some(false),
            workspace_room: Some(false),
            org_registry: Some(false),
            chat_rooms_live: 0,
            ..online()
        };
        assert!(health.dark());
        let summary = health.summary();
        assert!(summary.contains("0 of 5 live"), "{summary}");
        assert!(summary.contains("holds NO edge connections"), "{summary}");
    }

    /// The gh#126 shape: the doc room answers, the presence sub-room does not.
    /// `live()`/`dark()` stay clean — the sockets ARE up — but the summary must
    /// name it, because from every other device this engine reads "offline".
    #[test]
    fn dead_presence_on_a_live_room_is_named() {
        let health = EdgeHealth {
            workspace_presence: Some(false),
            ..online()
        };
        assert!(!health.dark());
        let summary = health.summary();
        assert!(summary.contains("presence dead on workspace room"), "{summary}");
        assert!(summary.contains("read offline elsewhere"), "{summary}");

        let both = EdgeHealth {
            workspace_presence: Some(false),
            org_presence: Some(false),
            ..online()
        };
        assert!(
            both.summary()
                .contains("presence dead on workspace room, org registry"),
            "{}",
            both.summary()
        );
    }

    /// A room that is DOWN carries no presence complaint — the room being down
    /// is the story, and "presence dead" on top of it would be noise.
    #[test]
    fn a_down_room_does_not_double_report_presence() {
        let health = EdgeHealth {
            workspace_room: Some(false),
            workspace_presence: Some(false),
            ..online()
        };
        assert!(!health.summary().contains("presence dead"));
    }

    /// An engine wired to no edge holds nothing on purpose — never a complaint.
    #[test]
    fn local_mode_is_never_dark() {
        let health = EdgeHealth::default();
        assert!(!health.dark());
        assert!(health.summary().contains("not syncing"));
    }

    /// The gh#483 shape, and the reason this crate learned a second axis: every
    /// socket is up — `live()` says so, `dark()` says no — while a chat room
    /// holds 74 transcript entries the edge has never taken. The summary must
    /// say that in the same breath as "5 of 5 live", because an operator
    /// reading only the first half concluded the Mac was fine for a week.
    #[test]
    fn a_live_engine_with_unconverged_content_says_so() {
        let health = EdgeHealth {
            chat_rooms_unconverged: 1,
            unacknowledged_entries: 74,
            unconverged_rooms: vec![RoomConvergence {
                chat_id: "b5b3c796".into(),
                state: "pending 74".into(),
                unacknowledged_entries: 74,
            }],
            ..online()
        };
        assert!(!health.dark(), "the sockets really are up");
        assert!(health.live_but_unconverged());
        let summary = health.summary();
        assert!(summary.contains("5 of 5 live"), "{summary}");
        assert!(
            summary.contains("1 chat room(s) not converged"),
            "{summary}"
        );
        assert!(summary.contains("74 local entr(ies)"), "{summary}");
        assert!(
            summary.contains("[b5b3c796: pending 74]"),
            "the room is NAMED, not only counted: {summary}"
        );
    }

    #[test]
    fn a_blocked_room_outranks_a_merely_pending_one() {
        let health = EdgeHealth {
            chat_rooms_unconverged: 2,
            chat_rooms_blocked: 1,
            chat_rooms_recovering: 1,
            unacknowledged_entries: 74,
            ..online()
        };
        let summary = health.summary();
        assert!(summary.contains("BLOCKED/local-only"), "{summary}");
        assert!(summary.contains("1 recovering"), "{summary}");
        assert!(
            !summary.contains("not converged"),
            "blocked is the headline, not a second clause: {summary}"
        );
    }

    /// Converged is the quiet case: no content clause at all.
    #[test]
    fn a_converged_engine_adds_no_content_clause() {
        let summary = online().summary();
        assert!(!summary.contains("converged"), "{summary}");
        assert!(!online().live_but_unconverged());
    }

    /// An edge with nothing attempted against it: no connections are
    /// *expected*, so there is nothing wrong to report either.
    #[test]
    fn signed_out_is_not_dark() {
        let health = EdgeHealth {
            edge_url: Some("https://edge.example".into()),
            ..EdgeHealth::default()
        };
        assert!(!health.dark());
        assert!(health.summary().contains("signed out"));
    }
}
