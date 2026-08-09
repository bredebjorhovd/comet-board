# Presence tells the truth, or says it cannot know — **done** (gh#126)

Eight space rows read "@Tokenmaxxer9000 · offline" (amber) while the box's
orchestrator had run for eight hours. Live diagnosis found the actual outage
one layer down: the Cloudflare account is on the Workers **free** plan, and the
15s presence beats keep the workspace/org Durable Objects permanently awake
(they can never hibernate), so the free tier's daily DO *duration* allowance
burns out mid-afternoon — from then until the daily reset the edge answers
every DO request with a 500 (`Exceeded allowed duration in Durable Objects
free tier`, caught live via `wrangler tail`). All rooms on all devices die at
once: the box goes genuinely unreachable, the Mac goes deaf, and the sidebar's
loudest signal accuses the box. The plan upgrade is the operational cure; the
code changes make the *label* stop lying whatever the outage:

- **The read is three-state now** (`comet_proto::view::host_presence`, pure +
  tested): a lapsed heartbeat is `Offline` (amber) only while THIS viewer's
  engine can hear — at least one sync room live. Deaf (both rooms down, signed
  out, local mode) renders `SyncDown` — "@ box · sync down", muted — indicting
  the pipe, which is the thing the viewer actually knows is broken. The gpui
  app polls its own engine's `EdgeHealth` every 15s to know which it is. One
  presence window (70s) is now shared by every surface via
  `view::PRESENCE_STALE_MS` — the TUI's was 45s, a real cross-surface
  disagreement.
- **The beat lost its silent failure mode** (`crates/sync/src/room.rs`): the
  `%EPH` presence sub-join was fire-and-forget — sent once per session; a
  `JoinError` only warned, an unanswered join left `joined_eph` false forever,
  and every outbound heartbeat was then dropped while doc sync stayed
  perfectly healthy. The session now re-sends the join every 15s until it
  lands (and on liveness-probe answers). Room test: swallow the join twice,
  presence must still come up.
- **The census names presence** (gh#116's `EdgeHealth`): new
  `workspacePresence`/`orgPresence` fields, read from the room clients' new
  `%EPH`-joined flag. `summary()` calls out "presence dead on … (doc sync is
  up; this device will read offline elsewhere)" — the wedge that used to be
  indistinguishable from 4-of-4-live. `comet status` and doctor inherit it.
- **The exit criterion is a test**: the gh#116 fake edge now broadcasts room
  frames between members, and a two-engine test (box + viewer, same user)
  asserts the box's heartbeat lands in the viewer's device row, survives a
  full edge redeploy unattended, and carries the presence census the whole
  way (`edge_reconnect.rs`).

The per-row device-suffix rendering itself still moves with gh#124; this issue
fixed what the suffix is allowed to claim.
