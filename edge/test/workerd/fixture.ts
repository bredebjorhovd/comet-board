import { DurableObject } from "cloudflare:workers";

/** Bare SQLite-backed DO; tests reach its real `ctx.storage.sql` via
 * `runInDurableObject`. Deliberately not SessionRoom/DeviceRoom: those import
 * loro-crdt, whose wasm cannot be compiled in the pool's test runner (see
 * vitest.workerd.config.ts). The storage underneath is the same real SQLite. */
export class TestLogRoom extends DurableObject {}

export default {
  fetch(): Response {
    return new Response("test fixture", { status: 404 });
  }
};
