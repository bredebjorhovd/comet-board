import { describe, expect, it } from "vitest";
import { acceptGate, emailValid, inviteGate, normalizeEmail } from "./invites";

// gh#76. Adding a teammate was a manual membership creation in the WorkOS
// dashboard. These are the rules the two new routes hang on: each one is a
// hole in someone's workspace if it inverts.

describe("invite gate", () => {
  const admin = { userId: "user_a", roleSlug: "admin" };
  const member = { userId: "user_b", roleSlug: "member" };

  it("lets an admin (or owner) invite", () => {
    expect(inviteGate([admin, member], "user_a")).toBe("allow");
    expect(inviteGate([{ userId: "user_a", roleSlug: "owner" }, member], "user_a")).toBe("allow");
  });

  it("refuses a plain member of a staffed org", () => {
    expect(inviteGate([admin, member], "user_b")).toBe("not-admin");
  });

  it("refuses someone who is not in the org at all", () => {
    // The token's org claim says which org the session is scoped to, not that
    // the caller still belongs to it — a removed member keeps their claim
    // until the next refresh.
    expect(inviteGate([admin, member], "user_c")).toBe("not-a-member");
  });

  it("lets the sole member invite whatever role they ended up with", () => {
    // `createOrg` falls back to the default role when the environment has no
    // "admin" slug; refusing here would wall the creator out of their own
    // workspace forever.
    expect(inviteGate([{ userId: "user_a", roleSlug: "member" }], "user_a")).toBe("allow");
    expect(inviteGate([{ userId: "user_a", roleSlug: null }], "user_a")).toBe("allow");
    // Still only for the member themselves.
    expect(inviteGate([{ userId: "user_a", roleSlug: "member" }], "user_b")).toBe("not-a-member");
  });

  it("refuses everyone when the org has no members", () => {
    expect(inviteGate([], "user_a")).toBe("not-a-member");
  });
});

describe("accept gate", () => {
  const pending = { email: "Teammate@Example.com", state: "pending", organizationId: "org_1" };

  it("admits the person the invitation names, case-insensitively", () => {
    expect(acceptGate(pending, "teammate@example.com")).toBe("allow");
    expect(acceptGate(pending, "  TEAMMATE@example.com  ")).toBe("allow");
  });

  it("refuses anyone else holding the token", () => {
    // The token is a bearer credential; the email is what binds it to the
    // identity WorkOS verified.
    expect(acceptGate(pending, "someone@else.com")).toBe("wrong-email");
  });

  it("refuses an invitation that is no longer pending", () => {
    for (const state of ["accepted", "expired", "revoked"]) {
      expect(acceptGate({ ...pending, state }, "teammate@example.com")).toBe("not-pending");
    }
  });

  it("refuses an invitation with no organization", () => {
    // WorkOS invitations can be org-less (plain sign-up invites); there is
    // nothing to join, and a blank org must never match a room's blank org.
    expect(acceptGate({ ...pending, organizationId: null }, "teammate@example.com")).toBe("no-org");
  });
});

describe("email handling", () => {
  it("normalizes to trimmed lowercase", () => {
    expect(normalizeEmail("  Ada@Example.COM ")).toBe("ada@example.com");
  });

  it("keeps obvious junk out", () => {
    expect(emailValid("ada@example.com")).toBe(true);
    expect(emailValid("ada+comet@sub.example.co.uk")).toBe(true);
    expect(emailValid("ada@example")).toBe(false);
    expect(emailValid("ada example@x.com")).toBe(false);
    expect(emailValid("")).toBe(false);
    expect(emailValid("@example.com")).toBe(false);
  });
});
