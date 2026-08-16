import { DurableObject } from "cloudflare:workers";
import { AlarmArmer } from "../../src/alarm";

const DAY_MS = 24 * 60 * 60 * 1000;
export { DeviceRoom } from "../../src/device-room";

/** Bare SQLite-backed DO; tests reach its real `ctx.storage.sql` via
 * `runInDurableObject`. SessionRoom stays out of this fixture because its
 * loro-crdt wasm cannot be compiled in the pool's test runner (see
 * vitest.workerd.config.ts). DeviceRoom only needs loro-protocol, so it is
 * exported directly above for real hibernation WebSocket coverage. */
export class TestLogRoom extends DurableObject {}

/**
 * Real-runtime seam for SessionRoom's alarm armer. It deliberately contains
 * only the state transitions relevant to the alarm: a write makes backup work
 * owed, and a client event can revive a room after its retry budget gave up.
 * The shared AlarmArmer is the production implementation under test; keeping
 * loro-crdt out lets this run in the workerd pool (see the config comment).
 */
export class TestAlarmRoom extends DurableObject {
  private readonly dailyAlarm = new AlarmArmer(this.ctx.storage);

  async write(): Promise<void> {
    await this.ctx.storage.put("backupDirty", true);
    await this.dailyAlarm.armAfter(DAY_MS);
  }

  async setExistingAlarm(at: number): Promise<void> {
    await this.ctx.storage.setAlarm(at);
  }

  async giveUp(): Promise<void> {
    await this.ctx.storage.put("alarmGaveUp", true);
    await this.ctx.storage.deleteAlarm();
  }

  async revive(): Promise<boolean> {
    if ((await this.ctx.storage.get<boolean>("alarmGaveUp")) !== true) return false;
    await this.dailyAlarm.armAfter(DAY_MS);
    await this.ctx.storage.put("alarmGaveUp", false);
    return true;
  }

  async status(): Promise<{ backupDirty: boolean; gaveUp: boolean; alarm: number | null }> {
    const [backupDirty, gaveUp, alarm] = await Promise.all([
      this.ctx.storage.get<boolean>("backupDirty"),
      this.ctx.storage.get<boolean>("alarmGaveUp"),
      this.ctx.storage.getAlarm()
    ]);
    return { backupDirty: backupDirty === true, gaveUp: gaveUp === true, alarm };
  }
}

export default {
  fetch(): Response {
    return new Response("test fixture", { status: 404 });
  }
};
