export interface Env {
  SESSION_ROOMS: DurableObjectNamespace;
  DEVICE_ROOMS: DurableObjectNamespace;
  BLOBS: R2Bucket;
  /** Release artifacts (headless tarballs, dmgs, latest.txt) served at
   * /releases/* for the curl-install flow. */
  RELEASES: R2Bucket;
  WORKOS_CLIENT_ID: string;
  /** "workos" (verify AuthKit JWTs) or "dev" (bearer == userId, never prod). */
  AUTH_MODE: string;
  /** Optional overrides for the WorkOS trust anchor. */
  WORKOS_ISSUER?: string;
  WORKOS_JWKS_URL?: string;
  /** WorkOS secret API key (wrangler secret) — powers the absorbed /auth/*
   * routes (code exchange, refresh, orgs). Unset ⇒ those routes answer 501,
   * matching the old apps/server dev-mode behavior. */
  WORKOS_API_KEY?: string;
}

/** Header the Worker stamps on requests it forwards into DOs after verifying
 * the caller's JWT. DOs trust it blindly — they are only reachable through
 * the Worker (design §2: "DO never sees an unauthenticated frame"). */
export const AUTH_USER_HEADER = "x-comet-auth-user";

/** Header the Worker stamps with the caller's verified WorkOS org claim
 * (`org_id`), when the session carries one. Same trust model as
 * [`AUTH_USER_HEADER`]: the DO never sees an unverified value.
 *
 * This is what makes a room shareable ACROSS users of one org (gh#66): a DO
 * records the org that claimed it and can then admit any caller the Worker
 * vouches for as a member of that org, instead of only the claiming user. A
 * caller with no org claim leaves the header unset, and every gate falls back
 * to the pre-org owner-only rule. */
export const AUTH_ORG_HEADER = "x-comet-auth-org";

/** Header the Worker stamps on requests forwarded into org-wide doc rooms —
 * the per-user workspace doc (`ws4/{orgId}/{userId}`) and the org device
 * registry (`orgdev1/{orgId}`). Membership (JWT org claim == orgId) is
 * enforced at the Worker; the SessionRoom DO sees this and skips its per-chat
 * claim-on-first-join ownership discipline for the room. */
export const ROOM_KIND_HEADER = "x-comet-room-kind";
