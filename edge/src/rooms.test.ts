import { describe, expect, it } from "vitest";
import {
  ORG_DEVICE_GENERATION,
  WORKSPACE_GENERATION,
  orgDeviceRoom,
  workspaceRoom
} from "./rooms";
import { isConcurrentWriteRoom } from "./session-room";

// gh#148 — the ws3 → ws4 generation break.
//
// A room name is the DO's identity: `idFromName(name)`. Changing it abandons
// the old instance's storage, which is the entire point here (ws3's update log
// was left causally broken by the 2026-08-04 abort-thrash and could not be
// repaired in place). The doc itself is not lost, because the edge is not
// authoritative for it — every device holds a full local replica and re-seeds
// the virgin room on first join.
describe("workspace room names", () => {
  it("is on the ws4 generation — the 2026-08-04 incident break", () => {
    expect(workspaceRoom("org_123", "user_456")).toBe("ws4/org_123/user_456");
  });

  // Per-user, and the user id comes from the caller's own verified claim in
  // `index.ts`. Two members of one org get two rooms; that is the ws3 privacy
  // break, which ws4 inherits rather than replaces.
  it("stays per-user", () => {
    expect(workspaceRoom("org_1", "user_a")).not.toBe(workspaceRoom("org_1", "user_b"));
  });

  // The registry is a separate room with a separate lifetime. Bumping it in
  // sympathy would blank the index a teammate needs to see the box at all
  // (gh#66) — and it never carried ws3's damage.
  it("does not drag the org device registry along on a workspace bump", () => {
    expect(orgDeviceRoom("org_123")).toBe("orgdev1/org_123");
    expect(ORG_DEVICE_GENERATION).toBe(1);
  });
});

// THE regression this ticket exists to make impossible.
//
// `isConcurrentWriteRoom` decides BY NAME which rooms are too
// concurrently-written to force-trim at the live frontier. Upstream wrote that
// guard against the literal `ws3/`; the moment the room became `ws4/` it
// silently stopped matching, and the next force-trim shallow-locked the room
// mid-flight and pushed in-flight peers into the penalty box — the same
// incident, twice, with no error anywhere (upstream 4aacc6d; ours `3809996`,
// extended to the registry in `d864d36`).
//
// So the guard and the name generator are asserted to agree HERE, rather than
// left to whoever bumps the counter next. A future ws5 that forgets this fails
// this test instead of production.
describe("a generation bump keeps its trim protection", () => {
  it("protects whatever generation the workspace room is currently on", () => {
    expect(isConcurrentWriteRoom(workspaceRoom("org_123", "user_456"))).toBe(true);
  });

  it("protects whatever generation the registry is currently on", () => {
    expect(isConcurrentWriteRoom(orgDeviceRoom("org_123"))).toBe(true);
  });

  // Both counters, swept forward. The guard is generation-agnostic by regex;
  // this is the proof that stays true for bumps nobody has made yet.
  it("holds for every generation either counter could reach", () => {
    for (let n = WORKSPACE_GENERATION; n <= WORKSPACE_GENERATION + 6; n++) {
      expect(isConcurrentWriteRoom(`ws${n}/org_123/user_456`)).toBe(true);
    }
    for (let n = ORG_DEVICE_GENERATION; n <= ORG_DEVICE_GENERATION + 6; n++) {
      expect(isConcurrentWriteRoom(`orgdev${n}/org_123`)).toBe(true);
    }
  });
});
