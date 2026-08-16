import { env } from "cloudflare:test";
import { describe, expect, it } from "vitest";

const DAY_MS = 24 * 60 * 60 * 1000;

const room = (name: string) => env.TEST_ALARM.get(env.TEST_ALARM.idFromName(name));

describe("daily alarm arming in workerd", () => {
  it("survives the initiating event ending immediately after an accepted write", async () => {
    const stub = room("write-event-lifetime");
    const before = Date.now();

    // The RPC returning is the initiating event boundary. There is
    // intentionally no timer, microtask drain, or in-instance inspection
    // before the next event asks workerd what was durably recorded.
    await stub.write();

    const status = await stub.status();
    expect(status.backupDirty).toBe(true);
    expect(status.alarm).not.toBeNull();
    expect(status.alarm!).toBeGreaterThanOrEqual(before + DAY_MS);
  });

  it("preserves an alarm that already owns the retry schedule", async () => {
    const stub = room("existing-retry");
    const retryAt = Date.now() + 60_000;
    await stub.setExistingAlarm(retryAt);

    await stub.write();

    expect((await stub.status()).alarm).toBe(retryAt);
  });

  it("durably re-arms a room after its retry ladder gave up", async () => {
    const stub = room("revive-after-give-up");
    await stub.giveUp();
    expect(await stub.status()).toMatchObject({ gaveUp: true, alarm: null });

    const before = Date.now();
    await expect(stub.revive()).resolves.toBe(true);

    const status = await stub.status();
    expect(status.gaveUp).toBe(false);
    expect(status.alarm).not.toBeNull();
    expect(status.alarm!).toBeGreaterThanOrEqual(before + DAY_MS);
  });
});
