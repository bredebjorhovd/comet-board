/**
 * The rooms' `meta` key/value table.
 *
 * Shared by `SessionRoom` and `DeviceRoom` for one reason beyond deduplication:
 * **a write that stores a value already stored still costs a row written.**
 * `INSERT … ON CONFLICT DO UPDATE` makes SQLite perform the UPDATE regardless of
 * whether the value changed, and Cloudflare bills every updated row.
 *
 * That is not academic. `SessionRoom.recordLoroUpdates` sets `tailDirty`,
 * `backupDirty` and `postReset` on EVERY inbound update batch, unbatched (the
 * update-log rows themselves are debounced by `DO_FLUSH_MS`). Those three flags
 * are cleared only by a `/tail` read, the daily alarm, and a wedge-break reset
 * respectively — so in a burst of K update batches, 3K rows were written and
 * ~3(K−1) of them changed nothing. On 2026-08-12 that helped push the account
 * through the Workers Free plan's 100,000-rows-written/day cap (102,517 written,
 * 63,283 of them in one hour), after which every Durable Object call threw
 * "Exceeded allowed rows written in Durable Objects free tier." in the
 * constructor for six hours. See docs/board/gh-373-what-the-edge-was-burning.md.
 *
 * So `writeMeta` reads before it writes. The trade is deliberate and good on
 * both plans: a row read is 1/50th of a row write's daily allowance on the free
 * plan (5M vs 100k) and 1/1000th of its price on paid ($0.001/M vs $1.00/M).
 */

export const ensureMetaTable = (sql: SqlStorage): void => {
  sql.exec("CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)");
};

export const readMeta = (sql: SqlStorage, key: string): string | undefined => {
  const rows = [...sql.exec("SELECT value FROM meta WHERE key = ?", key)];
  return rows[0]?.value as string | undefined;
};

/** Store `value` under `key`. Returns whether a row was actually written — a
 * no-op restore of the value already there writes nothing and returns false. */
export const writeMeta = (sql: SqlStorage, key: string, value: string): boolean => {
  if (readMeta(sql, key) === value) return false;
  sql.exec(
    "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    key,
    value
  );
  return true;
};
