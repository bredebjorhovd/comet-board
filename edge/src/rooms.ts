/**
 * Durable Object room NAMES — the strings the Worker passes to
 * `idFromName()`, and therefore the only thing that decides which DO instance
 * (and which DO storage) a request lands in.
 *
 * These names are WORKER-INTERNAL. The URL a client dials is
 * `/workspace/{orgId}/ws`; nothing in it names a generation, and the room id
 * a client carries inside its own protocol frames is an echo label the DO
 * never routes on (see `SessionRoom.fetch`, which stamps `chatId` meta from
 * the Worker's query param at upgrade time, before any JoinRequest exists).
 * That decoupling is what makes a generation bump a transparent migration
 * rather than a flag day: an engine still saying `ws3/…` in its frames lands
 * in the ws4 DO anyway, and converges by re-uploading its local doc. It is
 * pinned by `rooms.test.ts` and by `crates/engine/tests/edge_reconnect.rs`,
 * whose fake edge likewise derives the room from the PATH.
 *
 * ── Why the generation counter exists ──────────────────────────────────────
 * A generation bump is a DESTRUCTIVE BREAK: the new name is a different DO,
 * with empty storage. It is how this system abandons storage it can no longer
 * trust, because the workspace doc is not authoritative on the edge — every
 * signed-in device holds a complete local replica (`orgs/{org}/{user}/`), so
 * a virgin room is re-seeded by the first device to join and merged into by
 * the rest. Nothing is copied server-side; nothing needs to be.
 *
 *   ws2 — the spaces overhaul (schema break, org-wide room)
 *   ws3 — the per-user privacy break (`ws2/{orgId}` → `ws3/{orgId}/{userId}`)
 *   ws4 — the 2026-08-04 incident break (gh#148, upstream fb6492c): the
 *         abort-thrash loop left ws3 storage with causally-broken update rows
 *         — acks outran the debounced flush, so each crash stranded a
 *         half-persisted generation — and not even `/reset-log` could make it
 *         trustworthy again. A name bump allocates virgin storage. Orphaned
 *         ws3 rooms are never dialed again and hibernate at ~zero cost.
 *
 * The org device registry is deliberately NOT bumped in lockstep. It is a
 * separate room with a separate lifetime, it did not carry ws3's damage, and
 * abandoning it would blank the one index a teammate needs before they can
 * address the box at all (gh#66) — the workspace doc cannot substitute for it,
 * being per-user by construction.
 *
 * ── The invariant a bump must not break ────────────────────────────────────
 * `isConcurrentWriteRoom` (session-room.ts) decides, BY NAME, which rooms are
 * too concurrently-written to force-trim at the live frontier. Upstream wrote
 * that guard against the literal `ws3/`; when the room became `ws4/` the guard
 * silently stopped matching and re-broke the incident it was written to fix
 * (upstream 4aacc6d, ours `3809996` + `d864d36`). Every name minted here must
 * therefore still satisfy that guard — asserted in `rooms.test.ts` rather than
 * left to whoever bumps the counter next.
 */

/** Current workspace-doc generation. Bumping this abandons every existing
 * workspace room's storage fleet-wide — see the file header before doing it. */
export const WORKSPACE_GENERATION = 4;

/** Current org-device-registry generation. Independent of the workspace
 * doc's: these rooms are abandoned for their own reasons, never in sympathy. */
export const ORG_DEVICE_GENERATION = 1;

/** The per-user workspace doc room (spaces, chats index, sessions, presence).
 * Per-user by construction: the Worker derives `userId` from the caller's own
 * verified claim, so teammates in one org can never address each other's
 * rooms. */
export const workspaceRoom = (orgId: string, userId: string): string =>
  `ws${WORKSPACE_GENERATION}/${orgId}/${userId}`;

/** The org-wide device registry room (gh#66): one room per org that every
 * member joins, each device writing only its own row. */
export const orgDeviceRoom = (orgId: string): string =>
  `orgdev${ORG_DEVICE_GENERATION}/${orgId}`;
