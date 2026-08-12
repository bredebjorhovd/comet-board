import { describe, expect, it } from "vitest";
import { ensureMetaTable, readMeta, writeMeta } from "./meta";

/** Minimal SqlStorage fake covering exactly the statements meta.ts issues, and
 * counting rows written the way Cloudflare bills them: every executed
 * INSERT/UPDATE is a row, whether or not the value differed. */
class FakeSql {
  values = new Map<string, string>();
  rowsWritten = 0;
  rowsRead = 0;

  exec(query: string, ...params: unknown[]): Iterable<Record<string, unknown>> {
    if (query.startsWith("CREATE TABLE")) return [];
    if (query.startsWith("SELECT value FROM meta")) {
      const value = this.values.get(params[0] as string);
      this.rowsRead += value === undefined ? 0 : 1;
      return value === undefined ? [] : [{ value }];
    }
    if (query.startsWith("INSERT INTO meta")) {
      this.rowsWritten += 1;
      this.values.set(params[0] as string, params[1] as string);
      return [];
    }
    throw new Error(`FakeSql: unhandled query: ${query}`);
  }
}

const asSql = (fake: FakeSql) => fake as unknown as SqlStorage;

describe("meta writes", () => {
  it("writes a row when the key is new", () => {
    const sql = new FakeSql();
    ensureMetaTable(asSql(sql));
    expect(writeMeta(asSql(sql), "tailDirty", "1")).toBe(true);
    expect(sql.rowsWritten).toBe(1);
    expect(readMeta(asSql(sql), "tailDirty")).toBe("1");
  });

  it("writes a row when the value changes", () => {
    const sql = new FakeSql();
    writeMeta(asSql(sql), "tailDirty", "1");
    expect(writeMeta(asSql(sql), "tailDirty", "0")).toBe(true);
    expect(sql.rowsWritten).toBe(2);
    expect(readMeta(asSql(sql), "tailDirty")).toBe("0");
  });

  // The gh#373 defect. Free tier allows 100,000 rows written/day; on
  // 2026-08-12 the edge wrote 102,517 and every Durable Object call threw in
  // its constructor for six hours afterwards.
  it("writes nothing when the value is already stored", () => {
    const sql = new FakeSql();
    writeMeta(asSql(sql), "tailDirty", "1");
    sql.rowsWritten = 0;
    for (let i = 0; i < 1000; i++) {
      expect(writeMeta(asSql(sql), "tailDirty", "1")).toBe(false);
    }
    expect(sql.rowsWritten).toBe(0);
    expect(readMeta(asSql(sql), "tailDirty")).toBe("1");
  });

  // What `recordLoroUpdates` does on every inbound update batch. Before the
  // guard this was 3 rows written per batch forever; now it is 3 for the first
  // batch of a burst and 0 for every batch after it.
  it("charges the update-batch flag trio once per burst, not once per batch", () => {
    const sql = new FakeSql();
    ensureMetaTable(asSql(sql));
    const recordBatch = () => {
      writeMeta(asSql(sql), "tailDirty", "1");
      writeMeta(asSql(sql), "backupDirty", "1");
      writeMeta(asSql(sql), "postReset", "0");
    };
    for (let i = 0; i < 500; i++) recordBatch();
    expect(sql.rowsWritten).toBe(3);

    // A /tail read clears tailDirty; the next batch re-dirties exactly that one.
    writeMeta(asSql(sql), "tailDirty", "0");
    sql.rowsWritten = 0;
    recordBatch();
    expect(sql.rowsWritten).toBe(1);
  });

  // The guard trades a write for a read, so the read must actually happen —
  // that is what makes the trade sound on both plans (5M reads/day vs 100k
  // writes/day on free; $0.001/M vs $1.00/M on paid).
  it("reads before writing rather than caching in memory", () => {
    const sql = new FakeSql();
    writeMeta(asSql(sql), "owner", "user_1");
    const readsBefore = sql.rowsRead;
    writeMeta(asSql(sql), "owner", "user_1");
    expect(sql.rowsRead).toBe(readsBefore + 1);
    // A value changed underneath us (a sibling write path) is still observed.
    sql.values.set("owner", "user_2");
    expect(writeMeta(asSql(sql), "owner", "user_1")).toBe(true);
  });

  it("returns undefined for a key that was never written", () => {
    const sql = new FakeSql();
    ensureMetaTable(asSql(sql));
    expect(readMeta(asSql(sql), "missing")).toBeUndefined();
  });
});
