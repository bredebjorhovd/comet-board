import { describe, expect, it } from "vitest";
import { deviceRoomAccess } from "./device-room";
import { chatRoomAccess } from "./session-room";

// gh#66. Both rooms were claim-on-first-join by USER id, which made a second
// person in the same org impossible: their laptop was refused by the edge
// before a frame ever reached the box (relay), and a chat the box dispatched on
// the team's behalf could never be opened by anyone but its owner. The rules
// below are what "org-shared" means, and each one is a hole if it inverts.

describe("device room access", () => {
  const ORG = "org_1";
  const claimed = { owner: "user_a", org: ORG };

  it("lets the device's own backend claim an unclaimed room", () => {
    expect(deviceRoomAccess({}, { userId: "user_a", orgId: ORG }, "host")).toBe("claim");
  });

  it("never lets a client claim an unclaimed room", () => {
    // Claiming from the client side would let anyone who guesses a device id
    // own the room its real host later needs.
    expect(deviceRoomAccess({}, { userId: "user_a", orgId: ORG }, "client")).toBe("deny");
  });

  it("admits the owner from any of their own devices", () => {
    expect(deviceRoomAccess(claimed, { userId: "user_a", orgId: ORG }, "client")).toBe("owner");
    expect(deviceRoomAccess(claimed, { userId: "user_a", orgId: ORG }, "host")).toBe("owner");
  });

  it("admits a teammate of the room's org as a client", () => {
    expect(deviceRoomAccess(claimed, { userId: "user_b", orgId: ORG }, "client")).toBe("member");
  });

  it("refuses a teammate trying to HOST someone else's device", () => {
    // The host socket IS the device; only the identity it runs under may be it.
    expect(deviceRoomAccess(claimed, { userId: "user_b", orgId: ORG }, "host")).toBe("deny");
  });

  it("refuses another org, and refuses a caller with no org claim", () => {
    expect(deviceRoomAccess(claimed, { userId: "user_b", orgId: "org_2" }, "client")).toBe("deny");
    expect(deviceRoomAccess(claimed, { userId: "user_b" }, "client")).toBe("deny");
  });

  it("stays owner-only while the room has no org recorded", () => {
    // Rooms claimed before gh#66 shipped: owner-only until the owner's next
    // request backfills the org, never open-by-default.
    const legacy = { owner: "user_a" };
    expect(deviceRoomAccess(legacy, { userId: "user_b", orgId: ORG }, "client")).toBe("deny");
    expect(deviceRoomAccess(legacy, { userId: "user_a", orgId: ORG }, "client")).toBe("owner");
  });
});

describe("chat room access", () => {
  const ORG = "org_1";
  const priv = { owner: "user_a", org: ORG };
  const shared = { owner: "user_a", org: ORG, shared: true };

  it("claims on first join and admits the owner afterwards", () => {
    expect(chatRoomAccess({}, { userId: "user_a", orgId: ORG })).toBe("claim");
    expect(chatRoomAccess(priv, { userId: "user_a", orgId: ORG })).toBe("owner");
  });

  it("keeps an unshared chat private to its owner, teammate or not", () => {
    // The whole point of the per-user workspace doc: being in the org does not
    // make someone's own sessions readable.
    expect(chatRoomAccess(priv, { userId: "user_b", orgId: ORG })).toBe("deny");
  });

  it("admits a teammate once the owner shares the chat with the org", () => {
    expect(chatRoomAccess(shared, { userId: "user_b", orgId: ORG })).toBe("member");
  });

  it("refuses a shared chat to another org and to org-less callers", () => {
    expect(chatRoomAccess(shared, { userId: "user_b", orgId: "org_2" })).toBe("deny");
    expect(chatRoomAccess(shared, { userId: "user_b" })).toBe("deny");
  });

  it("refuses a chat marked shared before an org was ever recorded", () => {
    // Belt and braces: `shared` alone must never admit anyone — an empty org
    // would otherwise match every org-less caller.
    expect(chatRoomAccess({ owner: "user_a", shared: true }, { userId: "user_b" })).toBe("deny");
  });
});
