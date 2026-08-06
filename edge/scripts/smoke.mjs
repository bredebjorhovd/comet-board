/**
 * End-to-end smoke test against a running `wrangler dev` instance
 * (AUTH_MODE=dev). Exercises the full design surface:
 *   1. two Loro peers join a session room and converge through the DO
 *   2. streamed text appends propagate live
 *   3. GET /tail returns the materialized L2 tail
 *   4. POST/GET /diff round-trips the sidecar
 *   5. ephemeral (%EPH) presence relays between peers
 *   6. device room relays client↔host frames and serves sidecar slots
 *   7. R2 attachments: PUT (hash verified) then GET
 *   8. workspace room (`ws3/{orgId}/{userId}`): one user's devices converge;
 *      teammates in the same org are isolated (per-user docs); wrong org 403
 *   9. org-shared visibility (gh#66): the org device registry is org-wide, a
 *      teammate relays through the box's device room, and a chat the owner
 *      shared is readable + writable by the org (and by nobody else)
 *  10. absorbed /auth routes: 501 without WORKOS_API_KEY; cli callback page
 *
 * Usage: node scripts/smoke.mjs [baseUrl]   (default http://127.0.0.1:27640)
 */
import { LoroDoc } from "loro-crdt";
import { LoroWebsocketClient } from "loro-websocket";
import { LoroAdaptor, LoroEphemeralAdaptor } from "loro-adaptors/loro";
import { createHash, randomUUID } from "node:crypto";

const base = process.argv[2] ?? "http://127.0.0.1:27640";
const wsBase = base.replace(/^http/, "ws");
const token = "smoke-user";
const chatId = `smoke-${randomUUID().slice(0, 8)}`;
const deviceId = `smokedev-${randomUUID().slice(0, 8)}`;
const orgId = `org-smoke-${randomUUID().slice(0, 8)}`;

const fail = (msg) => {
  console.error(`✗ ${msg}`);
  process.exit(1);
};
const ok = (msg) => console.log(`✓ ${msg}`);
const until = async (fn, what, ms = 8000) => {
  const start = Date.now();
  while (Date.now() - start < ms) {
    if (await fn()) return;
    await new Promise((r) => setTimeout(r, 50));
  }
  fail(`timeout waiting for ${what}`);
};

// ── health ────────────────────────────────────────────────────────────────
{
  const res = await fetch(`${base}/health`);
  const body = await res.json();
  if (!body.ok) fail("health");
  if (body.auth !== "dev") fail(`expected dev auth mode, got ${body.auth} — run wrangler dev with --var AUTH_MODE:dev`);
  ok("health (dev auth)");
}

// ── session room: two peers converge ─────────────────────────────────────
const sessionUrl = `${wsBase}/session/${chatId}/ws?token=${token}`;

const clientA = new LoroWebsocketClient({ url: sessionUrl });
await clientA.waitConnected();
const adaptorA = new LoroAdaptor();
await clientA.join({ roomId: chatId, crdtAdaptor: adaptorA });
const docA = adaptorA.getDoc();
docA.getMap("meta").set("chatId", chatId);
docA.getMap("meta").set("schemaVersion", 1);
const messagesA = docA.getList("messages");
const m1 = messagesA.insertContainer(0, new (await import("loro-crdt")).LoroMap());
m1.set("id", "m1");
m1.set("role", "user");
m1.set("createdAt", Date.now());
m1.set("deviceId", "peer-a");
docA.commit();
ok("peer A joined + wrote");

const clientB = new LoroWebsocketClient({ url: `${wsBase}/session/${chatId}/ws?token=${token}` });
await clientB.waitConnected();
const adaptorB = new LoroAdaptor();
await clientB.join({ roomId: chatId, crdtAdaptor: adaptorB });
const docB = adaptorB.getDoc();
await until(() => docB.getList("messages").length > 0, "peer B backfill");
ok("peer B backfilled through DO");

// live propagation A→B
const t0 = docA.getList("messages").get(0);
docA.getMap("meta").set("title", "smoke");
docA.commit();
await until(() => docB.getMap("meta").get("title") === "smoke", "live A→B");
ok("live update A→B");

// live propagation B→A
docB.getMap("meta").set("fromB", true);
docB.commit();
await until(() => docA.getMap("meta").get("fromB") === true, "live B→A");
ok("live update B→A");
void t0;

// ── wrong user rejected ───────────────────────────────────────────────────
{
  const res = await fetch(`${base}/tail/${chatId}?token=intruder`);
  if (res.status !== 403) fail(`intruder tail expected 403, got ${res.status}`);
  ok("ownership enforced (intruder 403)");
}

// ── tail ──────────────────────────────────────────────────────────────────
await new Promise((r) => setTimeout(r, 100));
{
  const res = await fetch(`${base}/tail/${chatId}?token=${token}`);
  if (res.status !== 200) fail(`tail status ${res.status}`);
  const tail = await res.json();
  if (tail.chatId !== chatId) fail(`tail chatId ${tail.chatId}`);
  if (tail.totalMessages < 1) fail("tail totalMessages");
  ok(`tail (${tail.totalMessages} messages)`);
}

// ── diff sidecar ──────────────────────────────────────────────────────────
{
  const diff = { chatId, deviceId: "peer-a", checkoutPath: "/tmp/x", patch: "diff --git a b", files: [], additions: 1, deletions: 0, truncated: false, publishedAt: Date.now() };
  const post = await fetch(`${base}/diff/${chatId}?token=${token}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(diff)
  });
  if (post.status !== 200) fail(`diff post ${post.status}`);
  const get = await fetch(`${base}/diff/${chatId}?token=${token}`);
  const body = await get.json();
  if (body.patch !== diff.patch) fail("diff round-trip");
  ok("diff sidecar round-trip");
}

// ── ephemeral presence ────────────────────────────────────────────────────
{
  const ephA = new LoroEphemeralAdaptor();
  await clientA.join({ roomId: chatId, crdtAdaptor: ephA });
  const ephB = new LoroEphemeralAdaptor();
  await clientB.join({ roomId: chatId, crdtAdaptor: ephB });
  ephA.getStore().set("presence:peer-a", { status: "busy" });
  await until(
    () => ephB.getStore().get("presence:peer-a")?.status === "busy",
    "ephemeral A→B"
  );
  ok("ephemeral presence relay");
}

// ── workspace room: per-user docs — one user's devices converge, teammates
//    in the same org are isolated ─────────────────────────────────────────
{
  // Dev-mode org claim: token `userId@orgId`. The room id is derived at the
  // edge from the caller's OWN user claim: `ws3/{orgId}/{userId}`.
  const roomA = `ws3/${orgId}/alice`;
  const deviceA1 = new LoroWebsocketClient({
    url: `${wsBase}/workspace/${orgId}/ws?token=alice@${orgId}`
  });
  await deviceA1.waitConnected();
  const wsAdaptorA1 = new LoroAdaptor();
  await deviceA1.join({ roomId: roomA, crdtAdaptor: wsAdaptorA1 });
  const wsDocA = wsAdaptorA1.getDoc();
  wsDocA.getMap("meta").set("chatId", roomA);
  wsDocA.getMap("chats").set("chat-1", { title: "hello" });
  wsDocA.commit();

  // A SECOND DEVICE of the same user joins the same per-user room and
  // backfills.
  const deviceA2 = new LoroWebsocketClient({
    url: `${wsBase}/workspace/${orgId}/ws?token=alice@${orgId}`
  });
  await deviceA2.waitConnected();
  const wsAdaptorA2 = new LoroAdaptor();
  await deviceA2.join({ roomId: roomA, crdtAdaptor: wsAdaptorA2 });
  await until(
    () => wsAdaptorA2.getDoc().getMap("chats").get("chat-1") !== undefined,
    "workspace second-device backfill"
  );
  ok("workspace room: one user's devices converge");

  // A TEAMMATE (same org, different user) lands in their OWN empty room —
  // alice's spaces/sessions must be invisible to bob.
  const memberB = new LoroWebsocketClient({
    url: `${wsBase}/workspace/${orgId}/ws?token=bob@${orgId}`
  });
  await memberB.waitConnected();
  const wsAdaptorB = new LoroAdaptor();
  await memberB.join({ roomId: `ws3/${orgId}/bob`, crdtAdaptor: wsAdaptorB });
  await new Promise((resolve) => setTimeout(resolve, 400)); // any (wrong) backfill gets a beat
  if (wsAdaptorB.getDoc().getMap("chats").get("chat-1") !== undefined) {
    fail("teammate must NOT see another user's workspace doc");
  }
  ok("workspace room: teammates isolated (per-user docs)");
  memberB.close();
  deviceA2.close();

  // Wrong org claim rejected at the Worker.
  const wrongOrg = await fetch(`${base}/workspace/${orgId}/tail?token=mallory@org-other`);
  if (wrongOrg.status !== 403) fail(`wrong-org tail expected 403, got ${wrongOrg.status}`);
  // No org claim at all is rejected too.
  const noOrg = await fetch(`${base}/workspace/${orgId}/tail?token=${token}`);
  if (noOrg.status !== 403) fail(`no-org tail expected 403, got ${noOrg.status}`);
  // A member can read the workspace tail (empty messages — shape only).
  const memberTail = await fetch(`${base}/workspace/${orgId}/tail?token=alice@${orgId}`);
  if (memberTail.status !== 200) fail(`member workspace tail ${memberTail.status}`);
  ok("workspace room: org membership enforced (403 for outsiders)");

  deviceA1.close();
}

// ── device room ───────────────────────────────────────────────────────────
{
  const { encodeDeviceFrame, decodeDeviceFrame } = await import("./device-frame.mjs");
  const host = new WebSocket(`${wsBase}/device/${deviceId}/ws?token=${token}&role=host`);
  host.binaryType = "arraybuffer";
  await new Promise((resolve, reject) => {
    host.onopen = resolve;
    host.onerror = reject;
  });
  const hostFrames = [];
  host.onmessage = (e) => {
    if (typeof e.data === "string") return;
    const frame = decodeDeviceFrame(new Uint8Array(e.data));
    hostFrames.push(frame);
    // echo rpc payloads back to the sender
    if (frame.header.k === "rpc" && frame.header.from) {
      host.send(
        encodeDeviceFrame(
          { s: frame.header.s, k: "rpc", to: frame.header.from },
          frame.payload
        )
      );
    }
  };

  const connId = "conn-1";
  const client = new WebSocket(`${wsBase}/device/${deviceId}/ws?token=${token}&role=client&connId=${connId}`);
  client.binaryType = "arraybuffer";
  await new Promise((resolve, reject) => {
    client.onopen = resolve;
    client.onerror = reject;
  });
  const clientFrames = [];
  client.onmessage = (e) => {
    if (typeof e.data === "string") return;
    clientFrames.push(decodeDeviceFrame(new Uint8Array(e.data)));
  };
  client.send(encodeDeviceFrame({ s: "rpc-1", k: "rpc" }, new TextEncoder().encode("hello-host")));
  await until(() => clientFrames.length > 0, "device rpc echo");
  const echoed = new TextDecoder().decode(clientFrames[0].payload);
  if (echoed !== "hello-host") fail(`device echo got ${echoed}`);
  ok("device room rpc echo (client→host→client)");

  // intruder cannot join the device room
  const evil = new WebSocket(`${wsBase}/device/${deviceId}/ws?token=evil&role=client`);
  const evilResult = await new Promise((resolve) => {
    evil.onopen = () => resolve("open");
    evil.onerror = () => resolve("error");
    setTimeout(() => resolve("timeout"), 3000);
  });
  if (evilResult === "open") fail("intruder joined device room");
  ok("device room ownership enforced");

  // sidecar slot
  const post = await fetch(`${base}/device/${deviceId}/sidecar/repos?token=${token}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ repos: [{ path: "/x", name: "x" }] })
  });
  if (post.status !== 200) fail(`sidecar post ${post.status}`);
  const got = await (await fetch(`${base}/device/${deviceId}/sidecar/repos?token=${token}`)).json();
  if (got.repos?.[0]?.name !== "x") fail("sidecar round-trip");
  ok("device sidecar slot round-trip");

  // nudge: live delivery to the connected host
  const nudge = await fetch(`${base}/device/${deviceId}/nudge?token=${token}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ chatId: "chat-live" })
  });
  if ((await nudge.json()).delivered !== true) fail("live nudge not delivered");
  await until(
    () => hostFrames.some((f) => f.header.k === "nudge" && new TextDecoder().decode(f.payload).includes("chat-live")),
    "live nudge frame"
  );
  ok("nudge delivered live to connected host");

  host.close();
  client.close();

  // nudge: queued while host offline, replayed on rejoin
  await new Promise((r) => setTimeout(r, 200)); // let the close land
  const queued = await fetch(`${base}/device/${deviceId}/nudge?token=${token}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ chatId: "chat-cold" })
  });
  if ((await queued.json()).queued !== true) fail("offline nudge not queued");
  const host2 = new WebSocket(`${wsBase}/device/${deviceId}/ws?token=${token}&role=host`);
  host2.binaryType = "arraybuffer";
  const replayed = [];
  host2.onmessage = (e) => {
    if (typeof e.data === "string") return;
    replayed.push(decodeDeviceFrame(new Uint8Array(e.data)));
  };
  await until(
    () => replayed.some((f) => f.header.k === "nudge" && new TextDecoder().decode(f.payload).includes("chat-cold")),
    "queued nudge replay on host join"
  );
  ok("nudge queued offline and replayed on host join");
  host2.close();
}

// ── org-shared visibility (gh#66): the three gates a second user hits ─────
//    1. the org device registry — a teammate can SEE the box at all
//    2. the device room — a teammate may relay through it
//    3. a shared chat — a teammate may open and write to a board-dispatched chat
{
  const { encodeDeviceFrame, decodeDeviceFrame } = await import("./device-frame.mjs");
  const alice = `alice@${orgId}`;
  const bob = `bob@${orgId}`;
  const outsider = `mallory@org-other-${randomUUID().slice(0, 8)}`;
  const boxId = `smokebox-${randomUUID().slice(0, 8)}`;
  const boardChat = `smokeboard-${randomUUID().slice(0, 8)}`;

  // ── gate 1: the org device registry is ONE room per org ────────────────
  const regA = new LoroWebsocketClient({
    url: `${wsBase}/org/${orgId}/devices/ws?token=${alice}`
  });
  await regA.waitConnected();
  const regAdaptorA = new LoroAdaptor();
  await regA.join({ roomId: `orgdev1/${orgId}`, crdtAdaptor: regAdaptorA });
  regAdaptorA.getDoc().getMap("devices").set(boxId, { id: boxId, name: "the box" });
  regAdaptorA.getDoc().commit();

  const regB = new LoroWebsocketClient({
    url: `${wsBase}/org/${orgId}/devices/ws?token=${bob}`
  });
  await regB.waitConnected();
  const regAdaptorB = new LoroAdaptor();
  await regB.join({ roomId: `orgdev1/${orgId}`, crdtAdaptor: regAdaptorB });
  await until(
    () => regAdaptorB.getDoc().getMap("devices").get(boxId) !== undefined,
    "teammate sees the org's device row"
  );
  ok("org devices: a teammate sees the box (gate 1)");

  {
    const wrongOrg = await fetch(`${base}/org/${orgId}/devices/tail?token=${outsider}`);
    if (wrongOrg.status !== 403) fail(`outsider devices tail expected 403, got ${wrongOrg.status}`);
    const noOrg = await fetch(`${base}/org/${orgId}/devices/tail?token=${token}`);
    if (noOrg.status !== 403) fail(`org-less devices tail expected 403, got ${noOrg.status}`);
    ok("org devices: outsiders and org-less callers 403");
  }
  regA.close();
  regB.close();

  // ── gate 2: the box's device room admits the org, not just its owner ───
  const boxHost = new WebSocket(`${wsBase}/device/${boxId}/ws?token=${alice}&role=host`);
  boxHost.binaryType = "arraybuffer";
  await new Promise((resolve, reject) => {
    boxHost.onopen = resolve;
    boxHost.onerror = reject;
  });
  boxHost.onmessage = (e) => {
    if (typeof e.data === "string") return;
    const frame = decodeDeviceFrame(new Uint8Array(e.data));
    if (frame.header.k === "rpc" && frame.header.from) {
      boxHost.send(
        encodeDeviceFrame({ s: frame.header.s, k: "rpc", to: frame.header.from }, frame.payload)
      );
    }
  };

  const mate = new WebSocket(`${wsBase}/device/${boxId}/ws?token=${bob}&role=client&connId=mate-1`);
  mate.binaryType = "arraybuffer";
  const mateJoin = await new Promise((resolve) => {
    mate.onopen = () => resolve("open");
    mate.onerror = () => resolve("error");
    setTimeout(() => resolve("timeout"), 3000);
  });
  if (mateJoin !== "open") fail(`teammate device-room join: ${mateJoin}`);
  const mateFrames = [];
  mate.onmessage = (e) => {
    if (typeof e.data === "string") return;
    mateFrames.push(decodeDeviceFrame(new Uint8Array(e.data)));
  };
  mate.send(encodeDeviceFrame({ s: "rpc-1", k: "rpc" }, new TextEncoder().encode("board-rpc")));
  await until(() => mateFrames.length > 0, "teammate rpc echo through the box");
  if (new TextDecoder().decode(mateFrames[0].payload) !== "board-rpc") fail("teammate echo bytes");
  ok("device room: a teammate relays through the box (gate 2)");

  {
    const evil = new WebSocket(`${wsBase}/device/${boxId}/ws?token=${outsider}&role=client`);
    const result = await new Promise((resolve) => {
      evil.onopen = () => resolve("open");
      evil.onerror = () => resolve("error");
      setTimeout(() => resolve("timeout"), 3000);
    });
    if (result === "open") fail("another org joined the box's device room");
    // Hosting is owner-only even inside the org: the host socket IS the device.
    const usurper = new WebSocket(`${wsBase}/device/${boxId}/ws?token=${bob}&role=host`);
    const usurped = await new Promise((resolve) => {
      usurper.onopen = () => resolve("open");
      usurper.onerror = () => resolve("error");
      setTimeout(() => resolve("timeout"), 3000);
    });
    if (usurped === "open") fail("a teammate hosted someone else's device room");
    ok("device room: other orgs refused, hosting stays owner-only");
  }
  boxHost.close();
  mate.close();

  // ── gate 3: chat rooms are private until the owner shares them ─────────
  const chatOwner = new LoroWebsocketClient({
    url: `${wsBase}/session/${boardChat}/ws?token=${alice}`
  });
  await chatOwner.waitConnected();
  const chatAdaptorA = new LoroAdaptor();
  await chatOwner.join({ roomId: boardChat, crdtAdaptor: chatAdaptorA });
  chatAdaptorA.getDoc().getMap("meta").set("chatId", boardChat);
  chatAdaptorA.getDoc().commit();

  {
    const before = await fetch(`${base}/tail/${boardChat}?token=${bob}`);
    if (before.status !== 403) fail(`unshared chat expected 403 for teammate, got ${before.status}`);
    ok("chat rooms stay private to their owner until shared");
  }

  const share = await fetch(`${base}/share/${boardChat}?token=${alice}`, { method: "POST" });
  if (share.status !== 200) fail(`share ${share.status}: ${await share.text()}`);
  if ((await share.json()).org !== orgId) fail("share did not record the caller's org");
  const notMine = await fetch(`${base}/share/${boardChat}?token=${bob}`, { method: "POST" });
  if (notMine.status !== 403) fail(`teammate share expected 403, got ${notMine.status}`);
  ok("chat sharing is the owner's call (POST /share)");

  {
    const after = await fetch(`${base}/tail/${boardChat}?token=${bob}`);
    if (after.status !== 200) fail(`shared chat expected 200 for teammate, got ${after.status}`);
    const outsiderTail = await fetch(`${base}/tail/${boardChat}?token=${outsider}`);
    if (outsiderTail.status !== 403) fail(`shared chat expected 403 for other org, got ${outsiderTail.status}`);
  }

  // The teammate joins the room itself and WRITES — steering a board-dispatched
  // run is a command entry in this doc, so read-only would not be enough.
  const chatMate = new LoroWebsocketClient({
    url: `${wsBase}/session/${boardChat}/ws?token=${bob}`
  });
  await chatMate.waitConnected();
  const chatAdaptorB = new LoroAdaptor();
  await chatMate.join({ roomId: boardChat, crdtAdaptor: chatAdaptorB });
  await until(
    () => chatAdaptorB.getDoc().getMap("meta").get("chatId") === boardChat,
    "teammate backfills the shared chat"
  );
  chatAdaptorB.getDoc().getMap("commands").set("cmd-1", { issuedBy: "bobs-laptop" });
  chatAdaptorB.getDoc().commit();
  await until(
    () => chatAdaptorA.getDoc().getMap("commands").get("cmd-1") !== undefined,
    "teammate's command reaches the host"
  );
  ok("shared chat: a teammate reads AND steers it (gate 3)");
  chatOwner.close();
  chatMate.close();
}

// ── attachments ───────────────────────────────────────────────────────────
{
  const bytes = new TextEncoder().encode(`attachment-${chatId}`);
  const hash = createHash("sha256").update(bytes).digest("hex");
  const put = await fetch(`${base}/attachments/${hash}?token=${token}`, {
    method: "PUT",
    headers: { "content-type": "image/png" },
    body: bytes
  });
  if (put.status !== 200) fail(`attachment put ${put.status}: ${await put.text()}`);
  const get = await fetch(`${base}/attachments/${hash}?token=${token}`);
  if (get.status !== 200) fail(`attachment get ${get.status}`);
  const round = new Uint8Array(await get.arrayBuffer());
  if (new TextDecoder().decode(round) !== `attachment-${chatId}`) fail("attachment bytes");
  const bad = await fetch(`${base}/attachments/${"0".repeat(64)}?token=${token}`, {
    method: "PUT",
    body: bytes
  });
  if (bad.status !== 400) fail(`hash mismatch expected 400, got ${bad.status}`);
  ok("R2 attachments (hash-verified put/get)");
}

// ── absorbed auth routes ──────────────────────────────────────────────────
{
  // Dev instances have no WORKOS_API_KEY: secret-bearing routes answer 501
  // (matching the old apps/server behavior when WorkOS is unconfigured).
  const exchange = await fetch(`${base}/auth/exchange`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ code: "test" })
  });
  if (exchange.status !== 501) fail(`auth exchange expected 501 in dev, got ${exchange.status}`);
  const refresh = await fetch(`${base}/auth/refresh`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ refreshToken: "test" })
  });
  if (refresh.status !== 501) fail(`auth refresh expected 501 in dev, got ${refresh.status}`);
  ok("auth exchange/refresh answer 501 without WORKOS_API_KEY");

  // The headless callback needs no WorkOS config: it just renders state.code.
  const cb = await fetch(`${base}/auth/cli/callback?code=abc123&state=xyz789`);
  if (cb.status !== 200) fail(`cli callback ${cb.status}`);
  const page = await cb.text();
  if (!page.includes("xyz789.abc123")) fail("cli callback paste code missing");
  const cbBad = await fetch(`${base}/auth/cli/callback`);
  if (cbBad.status !== 400) fail(`cli callback without code expected 400, got ${cbBad.status}`);
  ok("auth cli callback renders paste code");
}

// ── reconnect: new client with existing state catches up incrementally ───
{
  const clientC = new LoroWebsocketClient({ url: `${wsBase}/session/${chatId}/ws?token=${token}` });
  await clientC.waitConnected();
  const preSeeded = new LoroDoc();
  preSeeded.import(docA.export({ mode: "snapshot" }));
  const adaptorC = new LoroAdaptor(preSeeded);
  await clientC.join({ roomId: chatId, crdtAdaptor: adaptorC });
  docA.getMap("meta").set("afterC", 1);
  docA.commit();
  await until(() => adaptorC.getDoc().getMap("meta").get("afterC") === 1, "peer C incremental");
  ok("version-vector incremental join");
  clientC.close();
}

clientA.close();
clientB.close();
console.log("\nALL SMOKE TESTS PASSED");
process.exit(0);
