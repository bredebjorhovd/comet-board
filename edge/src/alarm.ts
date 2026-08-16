type AlarmStorage = Pick<DurableObjectStorage, "getAlarm" | "setAlarm">;

/**
 * Arms one Durable Object alarm without replacing one that is already set.
 *
 * Concurrent callers share the same storage operation. Besides saving a
 * read, that is what keeps two interleaved writes from both observing `null`
 * and letting the later write move the alarm. The shared promise is always
 * awaited by a caller, and failures are deliberately left to propagate.
 */
export class AlarmArmer {
  private arming: Promise<void> | undefined;

  constructor(private readonly storage: AlarmStorage) {}

  async armAfter(delayMs: number, now: () => number = Date.now): Promise<void> {
    if (this.arming) {
      await this.arming;
      return;
    }

    const arming = this.armWhenMissing(delayMs, now);
    this.arming = arming;
    try {
      await arming;
    } finally {
      if (this.arming === arming) this.arming = undefined;
    }
  }

  private async armWhenMissing(delayMs: number, now: () => number): Promise<void> {
    const existing = await this.storage.getAlarm();
    if (existing === null) await this.storage.setAlarm(now() + delayMs);
  }
}
