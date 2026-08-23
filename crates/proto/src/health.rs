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
    /// Chat rooms whose content has been unacknowledged past
    /// `comet_sync::UNACKNOWLEDGED_ALERT` while the socket stayed live.
    ///
    /// The subset of [`Self::chat_rooms_unconverged`] that is a FAULT rather
    /// than a moment (gh#527 review). A write in flight is what a healthy fleet
    /// looks like most seconds of the day; two minutes of it on a live socket
    /// is gh#483's incident happening again, and the difference is the only
    /// thing that lets a health check grade content at all without failing on
    /// every keystroke. Zero from an engine too old to report it, which is the
    /// right fallback: it cannot see the state, so it must not be failed for it.
    #[serde(default)]
    pub chat_rooms_stalled: usize,
    /// Chat rooms the edge refuses content from: live, and local-only.
    #[serde(default)]
    pub chat_rooms_blocked: usize,
    /// Semantic entries (transcript + ledger) across all open chats that exist
    /// only on this device.
    #[serde(default)]
    pub unacknowledged_entries: usize,
    /// Rooms that JOINED AND DIED at least once in the last hour (gh#527).
    ///
    /// The axis this census was missing. Everything above is a point-in-time
    /// reading, and on the evening of 2026-08-19 every one of them was green —
    /// "10 of 10 live", "14 of 14 live" — while the whole fleet was in a
    /// dial/die/redial loop and the phone held 22 rooms on maxed-out backoff.
    /// Nothing was wrong with the arithmetic: a room in a join-then-die loop
    /// genuinely IS joined a fair share of the instants you might ask about.
    /// A sample cannot see a sequence, so the sequence has to be counted
    /// separately — see `comet_sync::churn`.
    #[serde(default)]
    pub rooms_churning: usize,
    /// Joined sessions across all rooms that ended inside 30s in the last hour
    /// — the rate, not a total, so a healed fleet reads healthy again.
    #[serde(default)]
    pub sessions_died_young_last_hour: usize,
    /// The churning rooms, worst first, named for the same reason
    /// [`Self::unconverged_rooms`] are.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub churning_rooms: Vec<RoomChurn>,
    /// The rooms behind those counts, named. A count tells an operator that
    /// something is stuck; only the name tells them which chat to open — the
    /// same reason [`Self::summary`] names down connections instead of
    /// counting them. Converged rooms are omitted, so this is empty in the
    /// ordinary case and short in the bad one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unconverged_rooms: Vec<RoomConvergence>,
    /// Filesystem watcher instances the engine holds right now (gh#552).
    ///
    /// Every `notify` watcher is one OS instance — an inotify fd on Linux —
    /// against a per-user kernel cap (`fs.inotify.max_user_instances`), and
    /// nothing used to read the engine's share anywhere: a box sat at 103 of
    /// 128 until the kernel said EAGAIN and the engine panicked itself to
    /// death four times in 48h.
    #[serde(default)]
    pub fs_watchers_open: usize,
    /// The engine's own bound on [`Self::fs_watchers_open`] — deliberately
    /// under any sane kernel cap so it refuses a watch (one checkout degrades
    /// to repair-tick diffs) before the kernel refuses the process. Zero from
    /// an engine too old to report it.
    #[serde(default)]
    pub fs_watchers_limit: usize,
    /// Watchers refused lifetime-total because the engine was at its bound.
    #[serde(default)]
    pub fs_watchers_refused: u64,
    /// Watcher deaths survived lifetime-total — poll failures and their
    /// follow-up unwraps, which pre-gh#552 engines turned into a full crash
    /// and every sync room dropped with it.
    #[serde(default)]
    pub fs_watchers_degraded: u64,
}

/// One room that keeps dying, as the census sees it (gh#527).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomChurn {
    /// The room's own id — a chat id, or `ws4/…` / `orgdev1/…` for the two
    /// org-wide docs.
    pub room_id: String,
    /// Joined sessions that ended inside 30s in the last hour.
    pub died_young_last_hour: usize,
    /// Every joined session that ended in the last hour: the denominator, so
    /// "7 of 8" and "7 of 400" do not read the same.
    pub sessions_last_hour: usize,
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

    /// Content that is not merely in flight but STUCK: the edge refuses it, or
    /// it has gone unacknowledged past the alert threshold on a live socket.
    ///
    /// [`Self::live_but_unconverged`] is the honest description of the gh#483
    /// state and the right thing to SAY; this is the narrower thing a health
    /// check may FAIL on. The distinction is the whole reason doctor could
    /// grade churn but not convergence: "unacknowledged" includes the write
    /// somebody made half a second ago, and a check that failed on that would
    /// be red all day and read by nobody. Two minutes on a live socket, or an
    /// outright refusal, is a different claim — and it is the one that was true
    /// of an engine sitting at 18 of 18 live with 262 entries that existed
    /// nowhere else.
    ///
    /// This is the fleet-wide form of `ConvergenceState::needs_attention` —
    /// "blocked always; pending only once it has outlasted the alert
    /// threshold", which gh#483 already wrote per room. The rule was there; no
    /// health surface was asking it.
    pub fn content_stuck(&self) -> bool {
        self.chat_rooms_blocked > 0 || self.chat_rooms_stalled > 0
    }

    /// The state gh#527 went dark in: sockets that keep coming back and rooms
    /// that never stay up. Distinct from both [`Self::dark`] (nothing live at
    /// all) and [`Self::live_but_unconverged`] (live, and content stuck) — a
    /// churning fleet passes both of those checks while nothing a person types
    /// on a phone is ever answered. Every surface that renders an engine as
    /// healthy has to ask this too.
    pub fn churning(&self) -> bool {
        self.rooms_churning > 0
    }

    /// Watchers have died and been survived (gh#552). Not fatal by design —
    /// the repair tick keeps the data right — but it is a kernel-level fault
    /// an operator should see, and it is what used to be a crash.
    pub fn watchers_degraded(&self) -> bool {
        self.fs_watchers_degraded > 0
    }

    /// The engine is refusing new watches because it is at its own bound:
    /// every checkout opened from here on loses its live diff updates until
    /// some close. The pre-crash state of gh#552's box, now visible before
    /// anything panics.
    pub fn watchers_saturated(&self) -> bool {
        self.fs_watchers_limit > 0 && self.fs_watchers_open >= self.fs_watchers_limit
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
        // Churn, not sockets (gh#527). Placed FIRST among the qualifiers,
        // ahead of even the content clauses, because it is the one that
        // contradicts the number an operator has already read: "10 of 10 live"
        // followed by "and 10 of those rooms died 34 times in the last hour"
        // is the sentence that was missing on 2026-08-19.
        if self.churning() {
            detail.push_str(&format!(
                " — {} room(s) CHURNING: {} session(s) joined and died inside 30s in the last \
                 hour (the sockets keep coming back; the rooms do not stay up, so replies do \
                 not arrive)",
                self.rooms_churning, self.sessions_died_young_last_hour
            ));
            let named: Vec<String> = self
                .churning_rooms
                .iter()
                .take(3)
                .map(|room| format!("{}: {} deaths/h", room.room_id, room.died_young_last_hour))
                .collect();
            if !named.is_empty() {
                detail.push_str(&format!(" [{}", named.join("; ")));
                if self.churning_rooms.len() > named.len() {
                    detail.push_str(&format!(
                        "; +{} more",
                        self.churning_rooms.len() - named.len()
                    ));
                }
                detail.push(']');
            }
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
        } else if self.chat_rooms_stalled > 0 {
            // The distinction a health check can act on (gh#527 review): this
            // content is not in flight, it has been sitting on a LIVE socket
            // past the alert threshold. Said in its own words rather than
            // folded into "not converged", because the two grade differently
            // and an operator reading the line is deciding whether to look.
            detail.push_str(&format!(
                " — {} chat room(s) STALLED: {} local entr(ies) unacknowledged by the edge \
                 past the alert threshold on a live socket (this is not lag)",
                self.chat_rooms_stalled, self.unacknowledged_entries
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
        // Local resources (gh#552), said in the same line as the edge state:
        // an operator reading "12 of 12 live" on a box whose engine is
        // refusing watches — or surviving dead ones — is deciding whether to
        // look based on what this sentence contains.
        if self.watchers_saturated() {
            detail.push_str(&format!(
                " — filesystem watchers AT LIMIT ({}/{}): new checkouts will not be watched \
                 live and their diffs fall back to the 2-minute repair tick (raise \
                 COMET_MAX_FS_WATCHERS or close worktrees)",
                self.fs_watchers_open, self.fs_watchers_limit
            ));
        }
        if self.watchers_degraded() {
            detail.push_str(&format!(
                " — {} filesystem watcher(s) DEGRADED lifetime: their kernel event loops died \
                 and were survived instead of crashing the engine; affected diffs rely on the \
                 repair tick until their entries rebuild",
                self.fs_watchers_degraded
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

    /// The gh#527 shape, and the reason this crate learned a THIRD axis: every
    /// socket is up when asked — `live()` says 5 of 5, `dark()` says no,
    /// content is converged — and the rooms have died 34 times in the last
    /// hour. That engine reported itself healthy all evening while nothing a
    /// person typed on a phone was ever answered.
    #[test]
    fn a_live_engine_whose_rooms_keep_dying_says_so() {
        let health = EdgeHealth {
            rooms_churning: 4,
            sessions_died_young_last_hour: 34,
            churning_rooms: vec![
                RoomChurn {
                    room_id: "ws4/org/user".into(),
                    died_young_last_hour: 12,
                    sessions_last_hour: 12,
                },
                RoomChurn {
                    room_id: "b5b3c796".into(),
                    died_young_last_hour: 9,
                    sessions_last_hour: 10,
                },
            ],
            ..online()
        };
        assert!(!health.dark(), "the sockets really do keep coming back");
        assert!(!health.live_but_unconverged(), "and content is not the fault");
        assert!(health.churning());
        let summary = health.summary();
        assert!(summary.contains("5 of 5 live"), "{summary}");
        assert!(summary.contains("4 room(s) CHURNING"), "{summary}");
        assert!(summary.contains("34 session(s)"), "{summary}");
        assert!(
            summary.contains("[ws4/org/user: 12 deaths/h; b5b3c796: 9 deaths/h]"),
            "the rooms are NAMED, not only counted: {summary}"
        );
    }

    /// Churn is a RATE. A fleet that thrashed this morning and has been fine
    /// since must read clean, or the gauge becomes something operators learn
    /// to ignore.
    #[test]
    fn a_healed_fleet_adds_no_churn_clause() {
        let summary = online().summary();
        assert!(!summary.contains("CHURNING"), "{summary}");
        assert!(!online().churning());
    }

    /// The gh#527 review's finding: churn blindness was fixed and convergence
    /// blindness was not. `live_but_unconverged` is the honest description and
    /// the right thing to say; `content_stuck` is the narrower thing a health
    /// check may fail on, and the difference is a write in flight versus a
    /// write that has been in flight for two minutes on a live socket.
    #[test]
    fn content_in_flight_and_content_stuck_are_different_claims() {
        let in_flight = EdgeHealth {
            chat_rooms_unconverged: 1,
            unacknowledged_entries: 3,
            ..online()
        };
        assert!(in_flight.live_but_unconverged(), "it IS unconverged");
        assert!(
            !in_flight.content_stuck(),
            "…and a check that failed on this would be red on every keystroke"
        );

        let stalled = EdgeHealth {
            chat_rooms_unconverged: 1,
            chat_rooms_stalled: 1,
            unacknowledged_entries: 262,
            ..online()
        };
        assert!(stalled.content_stuck());
        let summary = stalled.summary();
        assert!(summary.contains("5 of 5 live"), "{summary}");
        assert!(summary.contains("1 chat room(s) STALLED"), "{summary}");
        assert!(summary.contains("262 local entr(ies)"), "{summary}");
        assert!(
            summary.contains("this is not lag"),
            "the line has to say which of the two it is: {summary}"
        );

        // Refusal needs no threshold: the edge has said no.
        let blocked = EdgeHealth {
            chat_rooms_unconverged: 1,
            chat_rooms_blocked: 1,
            unacknowledged_entries: 74,
            ..online()
        };
        assert!(blocked.content_stuck());
        assert!(blocked.summary().contains("BLOCKED/local-only"));

        // And an engine too old to report stalled-ness is not failed for a
        // state it cannot see.
        assert!(!online().content_stuck());
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

    /// The gh#552 shapes. An engine at its watcher bound — the state that
    /// box sat in at 103 of 128 kernel instances before anything panicked —
    /// and an engine that survived dead watchers, both have to read in the
    /// same line that renders "healthy" for every other axis.
    #[test]
    fn a_saturated_or_degraded_watcher_population_is_named() {
        let saturated = EdgeHealth {
            fs_watchers_open: 96,
            fs_watchers_limit: 96,
            ..online()
        };
        assert!(saturated.watchers_saturated());
        let summary = saturated.summary();
        assert!(summary.contains("AT LIMIT (96/96)"), "{summary}");
        assert!(summary.contains("COMET_MAX_FS_WATCHERS"), "{summary}");

        let degraded = EdgeHealth {
            fs_watchers_degraded: 2,
            ..online()
        };
        assert!(degraded.watchers_degraded());
        let summary = degraded.summary();
        assert!(
            summary.contains("2 filesystem watcher(s) DEGRADED"),
            "{summary}"
        );
    }

    /// The ordinary case adds no watcher clauses, and an engine too old to
    /// report any of it (zeros from serde defaults) is not failed for a state
    /// it cannot see.
    #[test]
    fn a_quiet_watcher_population_adds_no_clauses() {
        let health = online();
        assert!(!health.watchers_saturated());
        assert!(!health.watchers_degraded());
        let summary = health.summary();
        assert!(!summary.contains("AT LIMIT"), "{summary}");
        assert!(!summary.contains("DEGRADED"), "{summary}");
    }
}
