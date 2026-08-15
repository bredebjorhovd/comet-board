import { env, runInDurableObject } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import { createMetaStore } from "../../src/meta";

/**
 * Real-runtime proof of the gh#377 fix. src/meta.test.ts asserts against a
 * FakeSql whose `writes` counter is our own bookkeeping — it proves the store
 * *skips the INSERT statement* when the value is unchanged, not that skipping
 * it is what saves the billed row. This tier closes that gap with workerd's
 * own metering: every `SqlStorageCursor` carries a `rowsWritten` count
 * maintained by the runtime, the same accounting the platform's rows-written
 * metric is fed from.
 *
 * What this proves: workerd attributes zero written rows to a repeated
 * `set()` of an unchanged value, and >0 to first writes and real transitions
 * — i.e. the read-before-write guard removes the write at the runtime level,
 * not merely in our fake.
 *
 * What it does NOT prove: the shape of the Cloudflare bill. Billing counts
 * writes from every source (update-log rows, KV storage puts, internals), and
 * a local workerd is not the billing pipeline. It also cannot detect a write
 * issued outside the instrumented `exec` seam — but `exec` is the only way
 * createMetaStore touches storage, so the seam is total for this store.
 */

/** Pass createMetaStore a wrapper whose exec delegates to the real DO SQLite
 * but keeps every cursor, so the runtime's own rowsWritten can be summed. */
const instrument = (sql: SqlStorage) => {
  const cursors: SqlStorageCursor<Record<string, SqlStorageValue>>[] = [];
  const wrapped = {
    exec(query: string, ...bindings: unknown[]) {
      const cursor = sql.exec(query, ...bindings);
      cursors.push(cursor);
      return cursor;
    }
  } as SqlStorage;
  return { wrapped, rowsWritten: () => cursors.reduce((n, c) => n + c.rowsWritten, 0) };
};

// Each test gets its own DO (fresh storage) keyed by test name.
const inRoom = <T>(name: string, fn: (sql: SqlStorage) => T): Promise<T> => {
  const stub = env.TEST_LOG.get(env.TEST_LOG.idFromName(name));
  return runInDurableObject(stub, (_instance, state) => fn(state.storage.sql));
};

describe("meta store on real DO SQLite", () => {
  it("a repeated set of an unchanged value writes zero rows by workerd's own count", async () => {
    await inRoom("noop-repeat", (sql) => {
      const { wrapped, rowsWritten } = instrument(sql);
      const meta = createMetaStore(wrapped);
      meta.set("tailDirty", "1");
      const afterFirst = rowsWritten();
      // The first write must actually land as a written row — otherwise a
      // zero below would prove nothing about the guard.
      expect(afterFirst).toBeGreaterThan(0);
      for (let i = 0; i < 100; i++) meta.set("tailDirty", "1");
      expect(rowsWritten()).toBe(afterFirst);
      expect(meta.get("tailDirty")).toBe("1");
    });
  });

  it("a real transition still costs a written row", async () => {
    await inRoom("real-transition", (sql) => {
      const { wrapped, rowsWritten } = instrument(sql);
      const meta = createMetaStore(wrapped);
      meta.set("tailDirty", "1");
      const afterFirst = rowsWritten();
      meta.set("tailDirty", "0");
      expect(rowsWritten()).toBeGreaterThan(afterFirst);
      expect(meta.get("tailDirty")).toBe("0");
    });
  });

  it("the ON CONFLICT upsert alone would bill the no-op (what the guard is for)", async () => {
    await inRoom("upsert-bills", (sql) => {
      // The store's statement, issued bare: same value twice must report a
      // written row both times. If workerd ever stops metering no-op upserts,
      // the read-before-write guard stops paying for itself and this tier
      // should say so.
      sql.exec("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)");
      const upsert = () =>
        sql.exec(
          "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
          "tailDirty",
          "1"
        ).rowsWritten;
      expect(upsert()).toBeGreaterThan(0);
      expect(upsert()).toBeGreaterThan(0);
    });
  });
});
