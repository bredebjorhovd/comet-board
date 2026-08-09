import { describe, expect, it } from "vitest";
import { isConcurrentWriteRoom } from "./session-room";

// gh#146, carrying upstream b019439 + 4aacc6d.
//
// A force-trim re-exports the doc as a SHALLOW snapshot and throws away the op
// history before that point. On a single-owner chat room that is safe and was
// the cure for the 2026-08-04 whale imports. On a doc every device writes at
// once it is not: a peer whose next ops depend on the discarded history pushes
// InvalidUpdate forever, and a trim landing mid-rebuild shallow-locks the room
// before the fleet finishes re-uploading.
//
// Upstream shipped that guard matching the literal room name `ws3/`. When the
// room name later moved to `ws4`, the guard silently stopped matching and a
// live-frontier trim stranded in-flight peers again — the same incident, twice.
// Hence a generation-agnostic pattern, and hence this test.
describe("force-trim room classification", () => {
  it("protects the per-user workspace doc we actually run", () => {
    expect(isConcurrentWriteRoom("ws3/org_123/user_456")).toBe(true);
  });

  it("survives a workspace generation bump — the bug that re-broke it upstream", () => {
    expect(isConcurrentWriteRoom("ws4/org_123/user_456")).toBe(true);
    expect(isConcurrentWriteRoom("ws10/org_123/user_456")).toBe(true);
  });

  // The registry is ours, not upstream's (gh#66): one room per org that every
  // member's every device writes its own row into. Same hazardous shape as the
  // workspace doc, so it needs the same protection — upstream's pattern, ported
  // verbatim, would have left it force-trimming at the live frontier.
  it("protects the org device registry", () => {
    expect(isConcurrentWriteRoom("orgdev1/org_123")).toBe(true);
    expect(isConcurrentWriteRoom("orgdev2/org_123")).toBe(true);
  });

  // Single-owner rooms keep the immediate live-frontier trim: that is the whole
  // point of TRIM_FORCE_BYTES, and a whale import with no aged checkpoint would
  // otherwise re-materialize its full history into the wasm heap for days.
  it("leaves chat rooms trimmable at the live frontier", () => {
    expect(isConcurrentWriteRoom("s2/01JQ7X8Y9Z")).toBe(false);
    expect(isConcurrentWriteRoom("01JQ7X8Y9Z")).toBe(false);
    expect(isConcurrentWriteRoom(undefined)).toBe(false);
    expect(isConcurrentWriteRoom("")).toBe(false);
  });

  // Prefix-only, anchored: a chat room whose id merely starts with these
  // letters is not a workspace room.
  it("does not match on a coincidental prefix", () => {
    expect(isConcurrentWriteRoom("ws/org_123")).toBe(false);
    expect(isConcurrentWriteRoom("wsabc/org_123")).toBe(false);
    expect(isConcurrentWriteRoom("orgdev/org_123")).toBe(false);
    expect(isConcurrentWriteRoom("s2/ws3/not-a-workspace")).toBe(false);
  });
});
