import { defineConfig } from "vitest/config";
import { cloudflareTest } from "@cloudflare/vitest-pool-workers";

// Runtime-real test tier: runs inside actual workerd via
// @cloudflare/vitest-pool-workers, against a real SQLite-backed Durable
// Object, so platform limits like the ~2MB SQLITE_TOOBIG row cap (the
// 2026-08-05 whale sync freeze) and workerd's own rows-written metering are
// the runtime's, not FakeSql constants. `npm run test:workerd`.
//
// `wrangler.configPath` points at the real wrangler.jsonc so compatibility
// date and the loro-crdt base64 alias are read from the deployed config, not
// duplicated here. `main` overrides the entry with test/workerd/fixture.ts,
// which exposes the bare SQLite/alarm probes and the production SessionRoom
// and DeviceRoom. That keeps Loro export/import and hibernatable-handler
// regressions in workerd instead of only in the Node fake tier.
export default defineConfig({
  plugins: [
    cloudflareTest({
      main: "./test/workerd/fixture.ts",
      wrangler: { configPath: "./wrangler.jsonc" },
      miniflare: {
        durableObjects: {
          TEST_LOG: { className: "TestLogRoom", useSQLite: true },
          TEST_ALARM: { className: "TestAlarmRoom", useSQLite: true },
          DEVICE_ROOM: { className: "DeviceRoom", useSQLite: true },
          TEST_SESSION: { className: "SessionRoom", useSQLite: true }
        }
      }
    })
  ],
  test: {
    include: ["test/workerd/**/*.test.ts"]
  }
});
