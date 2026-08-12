import { describe, expect, it } from "vitest";
import { createMetaStore } from "./meta";

/** Minimal SqlStorage fake covering exactly the statements meta.ts issues, and
 * counting the two things Cloudflare bills separately: rows written and rows
 * read. A write here means "SQLite performed the INSERT/UPDATE" — which is what
 * gets billed, whether or not the value changed. */
class FakeSql {
  values = new Map<string, string>();
  writes = 0;
  reads = 0;

  exec(query: string, ...params: unknown[]): Iterable<Record<string, unknown>> {
    if (query.startsWith("CREATE TABLE")) return [];
    if (query.startsWith("SELECT value FROM meta")) {
      const key = params[0] as string;
      const value = this.values.get(key);
      if (value === undefined) return [];
      this.reads++;
      return [{ value }];
    }
    if (query.startsWith("INSERT INTO meta")) {
      this.writes++;
      this.values.set(params[0] as string, params[1] as string);
      return [];
    }
    throw new Error(`FakeSql: unhandled query: ${query}`);
  }
}

const asSql = (fake: FakeSql) => fake as unknown as SqlStorage;

const storeOn = (fake: FakeSql) => createMetaStore(asSql(fake));

/** The three flags SessionRoom.recordLoroUpdates sets on every inbound batch,
 * in the order it sets them. */
const recordBatch = (meta: ReturnType<typeof storeOn>): void => {
  meta.set("tailDirty", "1");
  meta.set("backupDirty", "1");
  meta.set("postReset", "0");
};

describe("meta store", () => {
  it("writes a key that has no row yet", () => {
    const sql = new FakeSql();
    const meta = storeOn(sql);
    meta.set("tailDirty", "1");
    expect(sql.writes).toBe(1);
    expect(meta.get("tailDirty")).toBe("1");
  });

  it("performs no write when the value is already stored", () => {
    const sql = new FakeSql();
    const meta = storeOn(sql);
    meta.set("tailDirty", "1");
    sql.writes = 0;
    for (let i = 0; i < 100; i++) meta.set("tailDirty", "1");
    expect(sql.writes).toBe(0);
    expect(meta.get("tailDirty")).toBe("1");
  });

  it("still writes when the value actually changes", () => {
    const sql = new FakeSql();
    const meta = storeOn(sql);
    meta.set("tailDirty", "1");
    meta.set("tailDirty", "0");
    meta.set("tailDirty", "1");
    expect(sql.writes).toBe(3);
    expect(meta.get("tailDirty")).toBe("1");
  });

  it("distinguishes an empty string from a missing row", () => {
    const sql = new FakeSql();
    const meta = storeOn(sql);
    expect(meta.get("lastTrimAt")).toBeUndefined();
    meta.set("lastTrimAt", ""); // what reset() stores
    expect(sql.writes).toBe(1);
    expect(meta.get("lastTrimAt")).toBe("");
    meta.set("lastTrimAt", "");
    expect(sql.writes).toBe(1);
  });

  it("keeps keys independent", () => {
    const sql = new FakeSql();
    const meta = storeOn(sql);
    meta.set("tailDirty", "1");
    meta.set("backupDirty", "1");
    expect(sql.writes).toBe(2);
    meta.set("tailDirty", "1");
    meta.set("backupDirty", "0");
    expect(sql.writes).toBe(3);
    expect(meta.get("tailDirty")).toBe("1");
    expect(meta.get("backupDirty")).toBe("0");
  });

  // ── the gh#377 claim ─────────────────────────────────────────────────────
  //
  // Before the read-before-write guard, K update batches into one room wrote
  // 3K meta rows — 3(K−1) of them storing values that were already stored.

  it("costs a constant number of rows for a burst of update batches", () => {
    const sql = new FakeSql();
    const meta = storeOn(sql);
    const K = 500;
    for (let i = 0; i < K; i++) recordBatch(meta);
    // Three flags, each written exactly once for the whole burst.
    expect(sql.writes).toBe(3);
    expect(sql.writes).toBeLessThan(3 * K);
    expect(meta.get("tailDirty")).toBe("1");
    expect(meta.get("backupDirty")).toBe("1");
    expect(meta.get("postReset")).toBe("0");
  });

  it("charges reads, not writes, for the repeats it drops", () => {
    const sql = new FakeSql();
    const meta = storeOn(sql);
    recordBatch(meta);
    sql.writes = 0;
    sql.reads = 0;
    recordBatch(meta);
    // A row read costs 1/50th of a row write's free-plan allowance and
    // 1/1000th of its paid price, so this trade is strictly better on both.
    expect(sql.writes).toBe(0);
    expect(sql.reads).toBe(3);
  });

  // ── the transitions the guard must not swallow ───────────────────────────

  it("lets /tail see the tailDirty transition after a burst", () => {
    const sql = new FakeSql();
    const meta = storeOn(sql);
    for (let i = 0; i < 10; i++) recordBatch(meta);
    // currentTail(): materialize, then clear.
    expect(meta.get("tailDirty")).toBe("1");
    meta.set("tailDirty", "0");
    expect(meta.get("tailDirty")).toBe("0"); // cache now serves
    // The next batch re-dirties it — otherwise /tail would serve a stale tail.
    recordBatch(meta);
    expect(meta.get("tailDirty")).toBe("1");
  });

  it("lets the daily alarm see the backupDirty transition after a burst", () => {
    const sql = new FakeSql();
    const meta = storeOn(sql);
    for (let i = 0; i < 10; i++) recordBatch(meta);
    // alarm(): backupDirty === "1" runs the backup, which then clears it.
    expect(meta.get("backupDirty")).toBe("1");
    meta.set("backupVV", "vv-1");
    meta.set("backupDirty", "0");
    // An idle day stops the alarm chain.
    expect(meta.get("backupDirty")).toBe("0");
    // And the next batch re-arms it.
    recordBatch(meta);
    expect(meta.get("backupDirty")).toBe("1");
  });

  it("lets a wedge-break reset see the postReset transition after a burst", () => {
    const sql = new FakeSql();
    const meta = storeOn(sql);
    for (let i = 0; i < 10; i++) recordBatch(meta);
    expect(meta.get("postReset")).toBe("0");
    // reset(): pause the R2 put until real state comes back.
    meta.set("updateBytes", "0");
    meta.set("checkpoints", "[]");
    meta.set("lastTrimAt", "");
    meta.set("postReset", "1");
    expect(meta.get("postReset")).toBe("1");
    // The first re-uploaded batch clears it, and only the first one pays.
    sql.writes = 0;
    recordBatch(meta);
    expect(meta.get("postReset")).toBe("0");
    expect(sql.writes).toBe(1); // tailDirty/backupDirty were still "1"
    sql.writes = 0;
    recordBatch(meta);
    expect(sql.writes).toBe(0);
  });

  it("does not swallow a reset that lands on an already-reset room", () => {
    const sql = new FakeSql();
    const meta = storeOn(sql);
    meta.set("postReset", "1");
    // A second wedge-break with no traffic in between: nothing to write, and
    // nothing changes — the R2 put stays paused, which is the intent.
    sql.writes = 0;
    meta.set("postReset", "1");
    expect(sql.writes).toBe(0);
    expect(meta.get("postReset")).toBe("1");
  });
});
