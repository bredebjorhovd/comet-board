// gh#527 — the socket census, against the real hibernation API.
//
// The unit tier pins the ledger's rules (socket-log.test.ts). What only real
// workerd can pin is the wiring they depend on: that an accepted socket's
// ledger id survives in the attachment the runtime serializes, that
// `webSocketClose` is called with the code and reason this room now records,
// and that a room which has lost sockets says so on `/stats` — the surface an
// operator reaches for mid-incident.

import { env, runInDurableObject } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import { AUTH_USER_HEADER, ROOM_KIND_HEADER } from "../../src/env";
import type { SessionRoom } from "../../src/session-room";

const USER_ID = "synthetic-user";

interface Census {
  accepted: number;
  closed: number;
  vanished: number;
  diedYoung: number;
  diedYoungLastHour: number;
  open: number;
  recent: { kind: string; code?: number; reason?: string; deviceId?: string; ageMs: number }[];
}

const stats = async (room: SessionRoom): Promise<{ sockets: Census }> => {
  const response = await room.fetch(
    new Request("https://room.invalid/stats", {
      headers: { [AUTH_USER_HEADER]: USER_ID, [ROOM_KIND_HEADER]: "workspace" }
    })
  );
  expect(response.status).toBe(200);
  return (await response.json()) as { sockets: Census };
};

describe("SessionRoom socket census on real workerd", () => {
  it("records a socket's death with the code the runtime hands it", async () => {
    const stub = env.TEST_SESSION.get(env.TEST_SESSION.idFromName("socket-census"));

    await runInDurableObject(stub, async (instance, state) => {
      const room = instance as unknown as SessionRoom;
      const upgraded = await room.fetch(
        new Request("https://room.invalid/ws?chatId=census-chat&deviceId=synthetic-phone", {
          headers: {
            Upgrade: "websocket",
            [AUTH_USER_HEADER]: USER_ID,
            [ROOM_KIND_HEADER]: "workspace"
          }
        })
      );
      expect(upgraded.status).toBe(101);

      const open = await stats(room);
      expect(open.sockets).toMatchObject({ accepted: 1, open: 1, closed: 0, vanished: 0 });

      // 1006 is what the phone reported all evening. It reaches the room as a
      // close event here; the deaths that DON'T reach it are the vanished ones
      // the next wake reconciles (socket-log.ts).
      const [server] = state.getWebSockets();
      expect(server).toBeDefined();
      await room.webSocketClose(server!, 1006, "abnormal closure", false);

      const closed = await stats(room);
      expect(closed.sockets).toMatchObject({ accepted: 1, closed: 1, open: 0 });
      // Young, because it died in the same breath as it was accepted — which
      // is the churn the engine's "N of N live" gauge could not see.
      expect(closed.sockets.diedYoung).toBe(1);
      expect(closed.sockets.diedYoungLastHour).toBe(1);
      expect(closed.sockets.recent[0]).toMatchObject({
        kind: "close",
        code: 1006,
        reason: "abnormal closure",
        deviceId: "synthetic-phone"
      });
    });
  });
});
