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
    /// `ws3/{orgId}/{userId}` — this user's private workspace room (chats,
    /// spaces, sessions, presence).
    #[serde(default)]
    pub workspace_room: Option<bool>,
    /// `orgdev1/{orgId}` — the org-wide device registry, the index a teammate
    /// needs before they can address this box.
    #[serde(default)]
    pub org_registry: Option<bool>,
    /// Chat docs open on this engine, and how many hold a live session room.
    #[serde(default)]
    pub chat_rooms_open: usize,
    #[serde(default)]
    pub chat_rooms_live: usize,
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
            org_registry: Some(true),
            chat_rooms_open: 2,
            chat_rooms_live: 2,
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

    /// An engine wired to no edge holds nothing on purpose — never a complaint.
    #[test]
    fn local_mode_is_never_dark() {
        let health = EdgeHealth::default();
        assert!(!health.dark());
        assert!(health.summary().contains("not syncing"));
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
