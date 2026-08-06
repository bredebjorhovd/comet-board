/**
 * The /auth/* HTTP surface absorbed from comet's apps/server:
 *
 *  - POST   /auth/exchange              — WorkOS code → tokens (see `workos.ts`).
 *  - POST   /auth/refresh               — WorkOS refresh → fresh tokens (org-scopable).
 *  - GET    /auth/orgs                  — the caller's active org memberships.
 *  - POST   /auth/orgs                  — create an org + first (admin) membership.
 *  - GET    /auth/orgs/:id/members      — the org's members (admin).
 *  - GET    /auth/orgs/:id/invites      — pending invitations (admin).
 *  - POST   /auth/orgs/:id/invites      — invite a teammate by email (admin).
 *  - DELETE /auth/orgs/:id/invites/:iid — revoke a pending invitation (admin).
 *  - POST   /auth/invites/accept        — redeem an invitation token.
 *  - GET    /auth/cli/callback          — headless sign-in: shows a paste-able code.
 *
 * Exchange/refresh/callback run BEFORE the bearer gate (the caller has no
 * access token yet); every other route verifies the bearer itself — the user
 * id is ALWAYS the token's `sub`, never request input: users manage their own
 * memberships and no one else's. The member routes (gh#76) gate on the org's
 * membership list rather than the token's `org_id` claim, which says which org
 * a session is scoped to but nothing about the role it holds there.
 *
 * Error mapping matches the old server: bad body 400, missing bearer 401,
 * WorkOS-off 501, rejected exchange/refresh 401 — plus 403 for a caller who
 * may not manage the org, and (for member routes) WorkOS's own 4xx passed
 * through as 400 with its message, since "already invited" is the caller's
 * problem to fix, not an outage.
 */
import { bearerFromRequest, verifyToken } from "./auth";
import type { Env } from "./env";
import { acceptGate, emailValid, inviteGate, normalizeEmail } from "./invites";
import type { OrgMember } from "./workos";
import {
  WorkOsAuthFailed,
  acceptInvitation,
  createOrg,
  exchange,
  getUser,
  invitationByToken,
  inviteMember,
  listInvitations,
  listMembers,
  listOrgs,
  refresh,
  revokeInvitation,
  workosStatus
} from "./workos";

const json = (value: unknown, status = 200): Response =>
  new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json" }
  });

const notConfigured = (): Response => json({ error: "workos not configured" }, 501);

const authFailed = (e: unknown): Response =>
  json({ error: e instanceof WorkOsAuthFailed ? e.message : "authentication failed" }, 401);

/** A rejected member-management call: WorkOS's message, at a status that says
 * whose problem it is (see the module docblock). */
const workosFailed = (e: unknown): Response =>
  json(
    { error: e instanceof WorkOsAuthFailed ? e.message : "workos request failed" },
    workosStatus(e)
  );

const unauthorized = (): Response => json({ error: "invalid or missing bearer token" }, 401);

const bodyJson = async <T>(request: Request): Promise<T | undefined> => {
  try {
    return (await request.json()) as T;
  } catch {
    return undefined;
  }
};

/** Handle an /auth/* route; undefined means "not an auth route". */
export const handleAuthRoute = async (
  request: Request,
  env: Env,
  url: URL
): Promise<Response | undefined> => {
  const parts = url.pathname.split("/").filter(Boolean);
  if (parts[0] !== "auth") return undefined;
  const apiKey = env.WORKOS_API_KEY;

  if (parts[1] === "exchange" && parts.length === 2 && request.method === "POST") {
    if (!apiKey) return notConfigured();
    const body = await bodyJson<{ code?: string }>(request);
    if (typeof body?.code !== "string") return json({ error: "missing code" }, 400);
    try {
      return json(await exchange(env, apiKey, body.code));
    } catch (e) {
      return authFailed(e);
    }
  }

  if (parts[1] === "refresh" && parts.length === 2 && request.method === "POST") {
    if (!apiKey) return notConfigured();
    const body = await bodyJson<{ refreshToken?: string; organizationId?: string }>(request);
    if (typeof body?.refreshToken !== "string") return json({ error: "missing refreshToken" }, 400);
    if (body.organizationId !== undefined && typeof body.organizationId !== "string") {
      return json({ error: "missing refreshToken" }, 400);
    }
    try {
      return json(await refresh(env, apiKey, body.refreshToken, body.organizationId));
    } catch (e) {
      return authFailed(e);
    }
  }

  if (parts[1] === "orgs" && parts.length === 2) {
    if (!apiKey) return notConfigured();
    const token = bearerFromRequest(request);
    const caller = token ? await verifyToken(env, token) : undefined;
    if (!caller) return unauthorized();
    if (request.method === "GET") {
      try {
        return json({ orgs: await listOrgs(apiKey, caller.userId) });
      } catch (e) {
        return authFailed(e);
      }
    }
    if (request.method === "POST") {
      const body = await bodyJson<{ name?: string }>(request);
      if (typeof body?.name !== "string") return json({ error: "missing name" }, 400);
      const trimmed = body.name.trim();
      if (trimmed.length === 0 || trimmed.length > 80) {
        return json({ error: "name must be 1-80 characters" }, 400);
      }
      try {
        return json(await createOrg(apiKey, caller.userId, trimmed));
      } catch (e) {
        return authFailed(e);
      }
    }
  }

  if (parts[1] === "orgs" && parts.length >= 4) {
    const routed = await memberRoute(request, env, apiKey, parts);
    if (routed) return routed;
  }

  if (parts[1] === "invites" && parts[2] === "accept" && parts.length === 3) {
    if (request.method === "POST") return acceptRoute(request, env, apiKey);
  }

  if (parts[1] === "cli" && parts[2] === "callback" && request.method === "GET") {
    return cliCallback(url);
  }

  return undefined;
};

// ---------------------------------------------------------------------------
// Members and invitations (gh#76)
// ---------------------------------------------------------------------------

/** The org's members, seen by the gate: role slug and nothing else. */
const gateView = (members: readonly OrgMember[]) =>
  members.map((m) => ({ userId: m.userId, roleSlug: m.role }));

const gateRefusal = (gate: "not-a-member" | "not-admin"): Response =>
  gate === "not-a-member"
    ? json({ error: "not a member of that workspace" }, 403)
    : json({ error: "only workspace admins can invite" }, 403);

/** `/auth/orgs/:id/members` and `/auth/orgs/:id/invites[/:iid]`. Returns
 * undefined for a path/method this doesn't own, so the caller can fall
 * through to the remaining routes (and ultimately the 404). */
const memberRoute = async (
  request: Request,
  env: Env,
  apiKey: string | undefined,
  parts: string[]
): Promise<Response | undefined> => {
  const section = parts[3];
  if (section !== "members" && section !== "invites") return undefined;
  if (!apiKey) return notConfigured();
  const token = bearerFromRequest(request);
  const caller = token ? await verifyToken(env, token) : undefined;
  if (!caller) return unauthorized();
  const orgId = parts[2] ?? "";

  // One membership fetch answers both "who is in this org" and "may the caller
  // manage it" — the gate needs the whole list either way.
  let members: OrgMember[];
  try {
    members = await listMembers(apiKey, orgId, section === "members");
  } catch (e) {
    return workosFailed(e);
  }
  const gate = inviteGate(gateView(members), caller.userId);
  if (gate === "not-a-member") return gateRefusal(gate);

  if (section === "members" && parts.length === 4 && request.method === "GET") {
    // Seeing the roster is every member's business; changing it is not.
    return json({ members, canInvite: gate === "allow" });
  }

  if (section !== "invites") return undefined;
  if (gate !== "allow") return gateRefusal(gate);

  if (parts.length === 4 && request.method === "GET") {
    try {
      return json({ invites: await listInvitations(apiKey, orgId) });
    } catch (e) {
      return workosFailed(e);
    }
  }

  if (parts.length === 4 && request.method === "POST") {
    const body = await bodyJson<{ email?: string; role?: string }>(request);
    if (typeof body?.email !== "string") return json({ error: "missing email" }, 400);
    const email = normalizeEmail(body.email);
    if (!emailValid(email)) return json({ error: "enter a valid email address" }, 400);
    if (body.role !== undefined && typeof body.role !== "string") {
      return json({ error: "role must be a string" }, 400);
    }
    // No local "already a member" check: this path holds memberships without
    // addresses (only the roster route resolves them), and WorkOS refuses a
    // duplicate itself — its message is passed through by `workosFailed`.
    try {
      const invitation = await inviteMember(apiKey, {
        organizationId: orgId,
        email,
        inviterUserId: caller.userId,
        roleSlug: body.role?.trim() || undefined
      });
      return json({ invitation });
    } catch (e) {
      return workosFailed(e);
    }
  }

  if (parts.length === 5 && request.method === "DELETE") {
    const invitationId = parts[4] ?? "";
    try {
      // Confirm the invitation is one of THIS org's: an invitation id says
      // nothing about which org it belongs to, and being an admin here must
      // not let anyone revoke an invitation somewhere else.
      const pending = await listInvitations(apiKey, orgId);
      if (!pending.some((i) => i.id === invitationId)) {
        return json({ error: "no such pending invitation" }, 404);
      }
      await revokeInvitation(apiKey, invitationId);
      return json({ ok: true });
    } catch (e) {
      return workosFailed(e);
    }
  }

  return undefined;
};

/**
 * `POST /auth/invites/accept` — redeem an invitation token for the signed-in
 * caller. This is the path for someone already using Comet on this machine;
 * anyone else follows the hosted link WorkOS mailed them.
 */
const acceptRoute = async (
  request: Request,
  env: Env,
  apiKey: string | undefined
): Promise<Response> => {
  if (!apiKey) return notConfigured();
  const bearer = bearerFromRequest(request);
  const caller = bearer ? await verifyToken(env, bearer) : undefined;
  if (!caller) return unauthorized();
  const body = await bodyJson<{ token?: string }>(request);
  const inviteToken = typeof body?.token === "string" ? body.token.trim() : "";
  if (!inviteToken) return json({ error: "missing token" }, 400);

  try {
    const invite = await invitationByToken(apiKey, inviteToken);
    if (!invite) return json({ error: "that invitation code is not valid" }, 400);
    const user = await getUser(apiKey, caller.userId);
    const organizationId = invite.organizationId;
    const gate = acceptGate(
      { email: invite.email, state: invite.state, organizationId },
      user.email
    );
    // `organizationId === null` is the "no-org" case the gate already refused;
    // repeating it here is what narrows the type for `acceptInvitation`.
    if (gate !== "allow" || organizationId === null) {
      if (gate === "wrong-email") {
        return json({ error: "that invitation is for a different email address" }, 403);
      }
      return json({ error: "that invitation is no longer valid" }, 400);
    }
    return json(await acceptInvitation(apiKey, { ...invite, organizationId }, caller.userId));
  } catch (e) {
    return workosFailed(e);
  }
};

// ---------------------------------------------------------------------------
// Headless sign-in callback
// ---------------------------------------------------------------------------

/** Query params land verbatim in the page — escape them. (WorkOS codes/states
 * are URL-safe tokens, but this URL accepts anything.) */
const escapeHtml = (s: string): string =>
  s
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");

const cliPage = (body: string): string => `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<meta name="robots" content="noindex" />
<title>Comet — sign in</title>
<style>
  body { margin: 0; min-height: 100vh; display: grid; place-items: center;
         background: #0a0a0a; color: #ededed;
         font: 15px/1.6 ui-sans-serif, system-ui, sans-serif; }
  main { max-width: 34rem; padding: 2rem; text-align: center; }
  h1 { font-size: 1.05rem; font-weight: 600; margin: 0 0 0.75rem; }
  p { color: #a1a1a1; margin: 0.25rem 0; }
  code#paste { display: block; margin: 1.25rem 0 0.75rem; padding: 0.9rem 1rem;
         background: #171717; border: 1px solid #2e2e2e; border-radius: 8px;
         font: 13px/1.5 ui-monospace, monospace; word-break: break-all;
         user-select: all; cursor: pointer; }
  button { margin-top: 0.25rem; padding: 0.45rem 1rem; border-radius: 8px;
         border: 1px solid #2e2e2e; background: #ededed; color: #0a0a0a;
         font: 500 13px ui-sans-serif, system-ui, sans-serif; cursor: pointer; }
</style>
</head>
<body><main>${body}</main></body>
</html>`;

const html = (body: string, status = 200): Response =>
  new Response(body, { status, headers: { "content-type": "text/html; charset=utf-8" } });

/**
 * The hosted OAuth callback for headless (paste-code) sign-in. Registered as a
 * WorkOS redirect URI; it does NOT exchange the code — it renders `state.code`
 * for the user to paste into the device that started the flow (`comet login`),
 * where the exchange runs so the tokens land on that machine. The state half
 * must match the pending sign-in there, so the paste is CSRF-checked at the
 * same point the loopback flow is.
 */
const cliCallback = (url: URL): Response => {
  const code = url.searchParams.get("code");
  const state = url.searchParams.get("state");
  const denied = url.searchParams.get("error");
  if (denied || !code || !state) {
    const detail = denied
      ? `Sign-in was not completed (${escapeHtml(denied)}).`
      : "This link is missing its sign-in code.";
    return html(
      cliPage(`<h1>Sign-in failed</h1><p>${detail}</p><p>Start again from your terminal.</p>`),
      400
    );
  }
  const paste = `${escapeHtml(state)}.${escapeHtml(code)}`;
  return html(
    cliPage(
      `<h1>Almost there</h1>
<p>Paste this code into the terminal that asked for it:</p>
<code id="paste">${paste}</code>
<button onclick="navigator.clipboard.writeText(document.getElementById('paste').textContent).then(()=>{this.textContent='Copied'})">Copy code</button>
<p style="margin-top:1rem;font-size:13px">This code expires in a few minutes and only works on the device that started sign-in.</p>`
    )
  );
};
