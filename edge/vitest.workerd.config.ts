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
// duplicated here. `main` overrides the entry with test/workerd/fixture.ts:
// the real entry (src/index.ts) imports SessionRoom and therefore loro-crdt;
// loro's wasm does NOT work in this tier — the pool's test runner
// evaluates modules where wasm codegen is disallowed, while a real worker
// compiles the base64-inlined module at startup (deployed edge and
// `wrangler dev` are fine). The fixture exports DeviceRoom directly (it only
// imports loro-protocol) and keeps TestLogRoom for real DO SQLite; loro-on-
// workerd coverage stays with the wrangler-dev scripts
// (scripts/whale-check.mjs, scripts/fold-check.mjs).
export default defineConfig({
  plugins: [
    cloudflareTest({
      main: "./test/workerd/fixture.ts",
      wrangler: { configPath: "./wrangler.jsonc" },
      miniflare: {
        durableObjects: {
          TEST_LOG: { className: "TestLogRoom", useSQLite: true },
          TEST_ALARM: { className: "TestAlarmRoom", useSQLite: true },
          DEVICE_ROOM: { className: "DeviceRoom", useSQLite: true }
        }
      }
    })
  ],
  test: {
    include: ["test/workerd/**/*.test.ts"]
  }
});
