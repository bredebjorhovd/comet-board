/// <reference types="@cloudflare/vitest-pool-workers" />

declare module "cloudflare:test" {
  interface ProvidedEnv {
    TEST_LOG: DurableObjectNamespace;
    TEST_ALARM: DurableObjectNamespace<import("./fixture").TestAlarmRoom>;
  }
}
