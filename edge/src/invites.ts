/**
 * Who may invite a teammate, and who may redeem an invitation — the pure rules
 * behind `/auth/orgs/:id/invites` and `/auth/invites/accept` (gh#76). Adding a
 * member used to mean creating an `organization_membership` by hand in the
 * WorkOS dashboard; these are the two gates that let the product do it.
 *
 * Kept separate from `workos.ts` (the REST client) so the rules are testable
 * without a network — same split as `deviceRoomAccess` in `device-room.ts`.
 */

/** One active membership of the org, as far as the gates care. */
export interface Member {
  readonly userId: string;
  readonly roleSlug: string | null;
}

/** Role slugs allowed to manage members. Slugs are per-environment WorkOS
 * config; these are the two AuthKit ships by default. */
const ADMIN_ROLES = new Set(["admin", "owner"]);

export type InviteGate = "allow" | "not-a-member" | "not-admin";

/**
 * May `callerUserId` invite into this org? Membership is checked here and not
 * from the caller's token: the `org_id` claim says which org the session is
 * scoped to, never which role it holds there.
 */
export const inviteGate = (members: readonly Member[], callerUserId: string): InviteGate => {
  const caller = members.find((m) => m.userId === callerUserId);
  if (!caller) return "not-a-member";
  if (caller.roleSlug !== null && ADMIN_ROLES.has(caller.roleSlug)) return "allow";
  // `createOrg` falls back to the environment's default role when no "admin"
  // slug exists, so a workspace creator can be holding a plain member role.
  // The sole member of an org IS its creator — refusing them would leave that
  // workspace with nobody who can ever invite anyone.
  if (members.length === 1) return "allow";
  return "not-admin";
};

/** WorkOS matches addresses case-insensitively, and accept compares one
 * against the caller's own — compare the same normal form on both sides. */
export const normalizeEmail = (raw: string): string => raw.trim().toLowerCase();

/** Deliberately loose (`x@y.z`, no whitespace): WorkOS is the real validator,
 * this only keeps obvious junk out of the pending-invite list. */
export const emailValid = (email: string): boolean => /^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(email);

/** The invitation fields the accept gate reads. */
export interface InviteFacts {
  readonly email: string;
  /** WorkOS invitation state: pending | accepted | expired | revoked. */
  readonly state: string;
  readonly organizationId: string | null;
}

export type AcceptGate = "allow" | "not-pending" | "wrong-email" | "no-org";

/**
 * An invitation is redeemable only by the person it names, and only while it
 * is pending. The email check is what stops a leaked or guessed token from
 * being redeemed by whoever happens to hold it — the token alone is a bearer
 * credential, the email binds it to an identity WorkOS already verified.
 */
export const acceptGate = (invite: InviteFacts, callerEmail: string): AcceptGate => {
  if (invite.state !== "pending") return "not-pending";
  if (!invite.organizationId) return "no-org";
  if (normalizeEmail(invite.email) !== normalizeEmail(callerEmail)) return "wrong-email";
  return "allow";
};
