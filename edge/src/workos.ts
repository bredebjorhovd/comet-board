/**
 * Minimal WorkOS User Management REST client — the fetch-based port of the
 * old apps/server `WorkOsAuth` service (which used @workos-inc/node; the
 * Worker keeps it SDK-free). This is the one place that holds the WorkOS
 * **API key** (a Worker secret). Device backends build the public authorize
 * URL themselves and delegate the secret-bearing steps here, so the key never
 * lands on a device.
 *
 * Without WORKOS_API_KEY configured the routes answer 501; in dev mode
 * backends use their userId as the bearer and never call these.
 */
import type { Env } from "./env";

const API = "https://api.workos.com";

/** Thrown for rejected WorkOS calls; the sign-in routes map it to 401 (same as
 * the old server's WorkOsAuthFailed). `status` carries WorkOS's own status so
 * the member routes can tell "you asked for something impossible" (422 on a
 * duplicate invite) from "WorkOS is down" — see [`workosStatus`]. */
export class WorkOsAuthFailed extends Error {
  readonly status?: number;

  constructor(message: string, status?: number) {
    super(message);
    this.status = status;
  }
}

export interface ExchangeResult {
  readonly user: {
    readonly id: string;
    readonly email: string;
    readonly firstName: string | null;
    readonly lastName: string | null;
  };
  readonly accessToken: string;
  readonly refreshToken: string;
}

export interface RefreshResult {
  readonly accessToken: string;
  readonly refreshToken: string;
}

export interface OrgMembership {
  readonly id: string;
  readonly organizationId: string;
  readonly name: string;
}

interface WireUser {
  id: string;
  email: string;
  first_name: string | null;
  last_name: string | null;
}

interface WireAuthResponse {
  user: WireUser;
  access_token: string;
  refresh_token: string;
}

interface WireMembership {
  id: string;
  user_id: string;
  organization_id: string;
  organization_name?: string | null;
  role?: { slug?: string | null } | null;
}

const failed = async (res: Response): Promise<never> => {
  let message = "authentication failed";
  try {
    const body = (await res.json()) as { message?: string; error_description?: string; error?: string };
    message = body.message ?? body.error_description ?? body.error ?? message;
  } catch {
    /* non-JSON error body */
  }
  throw new WorkOsAuthFailed(message, res.status);
};

/** Status for a route to answer with when WorkOS rejected the call: its own
 * 4xx means the request was wrong (already invited, already a member), so say
 * 400 and pass the message through; anything else is WorkOS failing us. */
export const workosStatus = (e: unknown): number => {
  const status = e instanceof WorkOsAuthFailed ? e.status : undefined;
  return status !== undefined && status >= 400 && status < 500 ? 400 : 502;
};

const get = async (apiKey: string, path: string): Promise<Response> =>
  fetch(`${API}${path}`, { headers: { authorization: `Bearer ${apiKey}` } });

const post = async (apiKey: string, path: string, body: unknown): Promise<Response> =>
  fetch(`${API}${path}`, {
    method: "POST",
    headers: {
      authorization: `Bearer ${apiKey}`,
      "content-type": "application/json"
    },
    body: JSON.stringify(body)
  });

/** `authenticateWithCode`: WorkOS code → tokens + user. */
export const exchange = async (env: Env, apiKey: string, code: string): Promise<ExchangeResult> => {
  const res = await fetch(`${API}/user_management/authenticate`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      client_id: env.WORKOS_CLIENT_ID,
      client_secret: apiKey,
      grant_type: "authorization_code",
      code
    })
  });
  if (!res.ok) return failed(res);
  const r = (await res.json()) as WireAuthResponse;
  return {
    user: {
      id: r.user.id,
      email: r.user.email,
      firstName: r.user.first_name,
      lastName: r.user.last_name
    },
    accessToken: r.access_token,
    refreshToken: r.refresh_token
  };
};

/** `authenticateWithRefreshToken`; passing `organizationId` scopes the session
 * to that org (the next access token carries `org_id`). */
export const refresh = async (
  env: Env,
  apiKey: string,
  refreshToken: string,
  organizationId?: string
): Promise<RefreshResult> => {
  const res = await fetch(`${API}/user_management/authenticate`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      client_id: env.WORKOS_CLIENT_ID,
      client_secret: apiKey,
      grant_type: "refresh_token",
      refresh_token: refreshToken,
      ...(organizationId ? { organization_id: organizationId } : {})
    })
  });
  if (!res.ok) return failed(res);
  const r = (await res.json()) as WireAuthResponse;
  return { accessToken: r.access_token, refreshToken: r.refresh_token };
};

/** The user's active organization memberships. */
export const listOrgs = async (apiKey: string, userId: string): Promise<OrgMembership[]> => {
  const params = new URLSearchParams({ user_id: userId, statuses: "active", limit: "100" });
  const res = await fetch(`${API}/user_management/organization_memberships?${params}`, {
    headers: { authorization: `Bearer ${apiKey}` }
  });
  if (!res.ok) return failed(res);
  const r = (await res.json()) as { data: WireMembership[] };
  return r.data.map((m) => ({
    id: m.id,
    organizationId: m.organization_id,
    name: m.organization_name ?? m.organization_id
  }));
};

/** Create an organization and make the user its first (admin) member. */
export const createOrg = async (
  apiKey: string,
  userId: string,
  name: string
): Promise<{ organizationId: string }> => {
  const orgRes = await post(apiKey, "/organizations", { name });
  if (!orgRes.ok) return failed(orgRes);
  const org = (await orgRes.json()) as { id: string };
  // The creator administers their workspace. Role slugs are per-environment
  // config, so fall back to the default role if "admin" doesn't exist rather
  // than failing the whole onboarding.
  const withRole = await post(apiKey, "/user_management/organization_memberships", {
    user_id: userId,
    organization_id: org.id,
    role_slug: "admin"
  });
  if (!withRole.ok) {
    const fallback = await post(apiKey, "/user_management/organization_memberships", {
      user_id: userId,
      organization_id: org.id
    });
    if (!fallback.ok) return failed(fallback);
  }
  return { organizationId: org.id };
};

// ---------------------------------------------------------------------------
// Members and invitations (gh#76)
// ---------------------------------------------------------------------------

export interface OrgMember {
  /** The membership id (not the user id) — what WorkOS deletes/updates. */
  readonly id: string;
  readonly userId: string;
  /** Null when the address could not be resolved (see [`listMembers`]). */
  readonly email: string | null;
  readonly role: string | null;
}

export interface Invitation {
  readonly id: string;
  readonly email: string;
  readonly state: string;
  readonly expiresAt: string | null;
  /** WorkOS's own hosted accept link — mailed to the invitee, and the path a
   * teammate who is not on this desktop yet takes. */
  readonly acceptUrl: string | null;
}

interface WireInvitation {
  id: string;
  email: string;
  state: string;
  expires_at?: string | null;
  accept_invitation_url?: string | null;
  organization_id?: string | null;
  role?: { slug?: string | null } | null;
}

const toInvitation = (i: WireInvitation): Invitation => ({
  id: i.id,
  email: i.email,
  state: i.state,
  expiresAt: i.expires_at ?? null,
  acceptUrl: i.accept_invitation_url ?? null
});

/** How many member addresses one request will resolve. Memberships are one
 * fetch each and Workers cap subrequests; past this the list keeps rendering
 * with ids instead of failing. */
const EMAIL_LOOKUP_CAP = 40;

/** A WorkOS user by id — the caller's own verified email address, for the
 * accept gate. */
export const getUser = async (
  apiKey: string,
  userId: string
): Promise<{ id: string; email: string }> => {
  const res = await get(apiKey, `/user_management/users/${encodeURIComponent(userId)}`);
  if (!res.ok) return failed(res);
  const r = (await res.json()) as WireUser;
  return { id: r.id, email: r.email };
};

/**
 * The org's active memberships. `withEmails` additionally resolves each
 * member's address (memberships carry only a user id); a lookup that fails
 * leaves that row's email null rather than failing the whole list — the
 * settings pane degrades to ids, it does not go blank.
 */
export const listMembers = async (
  apiKey: string,
  organizationId: string,
  withEmails = false
): Promise<OrgMember[]> => {
  const params = new URLSearchParams({
    organization_id: organizationId,
    statuses: "active",
    limit: "100"
  });
  const res = await get(apiKey, `/user_management/organization_memberships?${params}`);
  if (!res.ok) return failed(res);
  const r = (await res.json()) as { data: WireMembership[] };
  const members: OrgMember[] = r.data.map((m) => ({
    id: m.id,
    userId: m.user_id,
    email: null,
    role: m.role?.slug ?? null
  }));
  if (!withEmails) return members;
  const emails = await Promise.all(
    members
      .slice(0, EMAIL_LOOKUP_CAP)
      .map((m) =>
        getUser(apiKey, m.userId)
          .then((u) => u.email)
          .catch(() => null)
      )
  );
  return members.map((m, ix) => ({ ...m, email: emails[ix] ?? null }));
};

/** The org's invitations that are still outstanding. */
export const listInvitations = async (
  apiKey: string,
  organizationId: string
): Promise<Invitation[]> => {
  const params = new URLSearchParams({ organization_id: organizationId, limit: "100" });
  const res = await get(apiKey, `/user_management/invitations?${params}`);
  if (!res.ok) return failed(res);
  const r = (await res.json()) as { data: WireInvitation[] };
  return r.data.filter((i) => i.state === "pending").map(toInvitation);
};

/** Invite `email` into the org. WorkOS mails the invitation and owns its
 * expiry; `roleSlug` is dropped when the environment has no such role. */
export const inviteMember = async (
  apiKey: string,
  input: {
    organizationId: string;
    email: string;
    inviterUserId: string;
    roleSlug?: string;
  }
): Promise<Invitation> => {
  const body = {
    email: input.email,
    organization_id: input.organizationId,
    inviter_user_id: input.inviterUserId,
    ...(input.roleSlug ? { role_slug: input.roleSlug } : {})
  };
  const res = await post(apiKey, "/user_management/invitations", body);
  if (res.ok) return toInvitation((await res.json()) as WireInvitation);
  if (!input.roleSlug) return failed(res);
  // Same fallback as `createOrg`: role slugs are per-environment config, so a
  // missing role must not cost the invite.
  const plain = await post(apiKey, "/user_management/invitations", {
    email: input.email,
    organization_id: input.organizationId,
    inviter_user_id: input.inviterUserId
  });
  if (!plain.ok) return failed(plain);
  return toInvitation((await plain.json()) as WireInvitation);
};

export const revokeInvitation = async (apiKey: string, invitationId: string): Promise<void> => {
  const res = await post(
    apiKey,
    `/user_management/invitations/${encodeURIComponent(invitationId)}/revoke`,
    {}
  );
  if (!res.ok) return failed(res);
};

export interface TokenInvitation {
  readonly id: string;
  readonly email: string;
  readonly state: string;
  readonly organizationId: string | null;
  readonly roleSlug: string | null;
}

/** Look an invitation up by the token from its accept link. Returns undefined
 * when WorkOS knows no such token (a typo'd or already-cleaned-up code). */
export const invitationByToken = async (
  apiKey: string,
  token: string
): Promise<TokenInvitation | undefined> => {
  const res = await get(
    apiKey,
    `/user_management/invitations/by_token/${encodeURIComponent(token)}`
  );
  if (res.status === 404) return undefined;
  if (!res.ok) return failed(res);
  const i = (await res.json()) as WireInvitation;
  return {
    id: i.id,
    email: i.email,
    state: i.state,
    organizationId: i.organization_id ?? null,
    roleSlug: i.role?.slug ?? null
  };
};

/**
 * Redeem an invitation for an ALREADY signed-in user: create the membership
 * the invitation describes, then revoke the invitation so its token cannot be
 * replayed. (WorkOS's own accept flow runs through hosted AuthKit and needs a
 * browser session; there is no server-side "accept" call to make instead, so
 * the membership is created here — the caller must have passed `acceptGate`.)
 *
 * A revoke that fails is logged-over, not fatal: the membership already
 * exists, and the stale invitation can only be redeemed by the same person
 * who just used it.
 */
export const acceptInvitation = async (
  apiKey: string,
  invite: TokenInvitation & { organizationId: string },
  userId: string
): Promise<{ organizationId: string }> => {
  const res = await post(apiKey, "/user_management/organization_memberships", {
    user_id: userId,
    organization_id: invite.organizationId,
    ...(invite.roleSlug ? { role_slug: invite.roleSlug } : {})
  });
  if (!res.ok) return failed(res);
  try {
    await revokeInvitation(apiKey, invite.id);
  } catch {
    /* membership is created; a lingering invitation is harmless */
  }
  return { organizationId: invite.organizationId };
};
