import { DurableObject } from "cloudflare:workers";
import { AlarmArmer } from "../../src/alarm";

const DAY_MS = 24 * 60 * 60 * 1000;
export { DeviceRoom } from "../../src/device-room";
export { SessionRoom } from "../../src/session-room";

/** Bare SQLite-backed DO; tests reach its real `ctx.storage.sql` via
 * `runInDurableObject`. The fixture also re-exports the production
 * SessionRoom and DeviceRoom so their Loro and hibernation handlers run in
 * the same real workerd tier. */
export class TestLogRoom extends DurableObject {}

/**
 * Real-runtime seam for SessionRoom's alarm armer. It deliberately contains
 * only the state transitions relevant to the alarm: a write makes backup work
 * owed, and a client event can revive a room after its retry budget gave up.
 * The shared AlarmArmer is the production implementation under test; this
 * small seam isolates its durable state transitions from the room handlers.
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
