import assert from "node:assert/strict";
import test from "node:test";

import {
  CloudflareR2Client,
  POLICY_CONFIRMATION,
  deleteRelease,
  deletionMarkerKey,
  expectedArtifactKeys,
  planRetention,
  publicationMarkerKey,
} from "./release-retention.mjs";

const object = (key, ageDays = 0) => ({
  key,
  size: key.endsWith(".json") || key.endsWith(".txt") ? 20 : 100,
  last_modified: new Date(Date.now() - ageDays * 86_400_000).toISOString(),
});

const releaseObjects = (version, ageDays = 0) =>
  expectedArtifactKeys(version).map((key) => object(key, ageDays));

const archive = (...versions) =>
  new Map(
    versions.map((version) => [
      version,
      {
        draft: false,
        prerelease: false,
        assets: expectedArtifactKeys(version),
      },
    ]),
  );

const pointers = (version) => ({
  latestText: version,
  manifest: {
    version,
    files: Object.fromEntries(
      expectedArtifactKeys(version).map((key) => [
        key,
        { sha256: "a".repeat(64) },
      ]),
    ),
  },
});

const inventory = (...versions) => [
  object("latest.txt"),
  object("manifest.json"),
  ...versions.flatMap((version, index) =>
    releaseObjects(version, 500 + index * 100),
  ),
];

test("a quiet period never ages the current or four rollback releases out", () => {
  const versions = ["2.0.0", "1.9.0", "1.8.0", "1.7.0", "1.6.0"];
  const plan = planRetention({
    objects: inventory(...versions),
    ...pointers("2.0.0"),
    archive: archive(...versions),
  });
  assert.deepEqual(plan.rollbackVersions, ["1.9.0", "1.8.0", "1.7.0", "1.6.0"]);
  assert.deepEqual(plan.candidates, []);
  assert.ok(
    plan.releases.every((release) => !release.disposition.startsWith("delete")),
  );
});

test("only complete archived releases beyond the rollback window are candidates", () => {
  const versions = [
    "2.0.0",
    "1.9.0",
    "1.8.0",
    "1.7.0",
    "1.6.0",
    "1.5.0",
    "1.4.0",
  ];
  const objects = inventory(...versions);
  // 1.4.0 is old enough but incomplete. Its remaining objects must not be
  // swept as a side-effect of identifying the complete 1.5.0 set.
  const incompleteKeys = new Set(expectedArtifactKeys("1.4.0").slice(0, 2));
  const incompleteObjects = objects.filter(
    (entry) =>
      !entry.key.includes("comet-1.4.0") || incompleteKeys.has(entry.key),
  );
  const plan = planRetention({
    objects: incompleteObjects,
    ...pointers("2.0.0"),
    archive: archive(...versions),
  });
  assert.deepEqual(
    plan.candidates.map((candidate) => candidate.version),
    ["1.5.0"],
  );
  assert.equal(
    plan.releases.find((release) => release.version === "1.4.0").disposition,
    "protect-incomplete",
  );
});

test("pointer disagreement and an incomplete advertised release fail closed", () => {
  const objects = inventory("2.0.0");
  assert.throws(
    () =>
      planRetention({
        objects,
        latestText: "2.0.0",
        manifest: pointers("1.9.0").manifest,
        archive: archive("2.0.0"),
      }),
    /pointer disagreement/,
  );
  assert.throws(
    () =>
      planRetention({
        objects: objects.filter(
          (entry) => !entry.key.endsWith("macos-arm64.dmg"),
        ),
        ...pointers("2.0.0"),
        archive: archive("2.0.0"),
      }),
    /advertised release 2\.0\.0 is incomplete/,
  );
});

test("any interrupted publication blocks all deletion", () => {
  const versions = ["2.0.0", "1.9.0", "1.8.0", "1.7.0", "1.6.0", "1.5.0"];
  const plan = planRetention({
    objects: [...inventory(...versions), object(publicationMarkerKey("2.1.0"))],
    ...pointers("2.0.0"),
    archive: archive(...versions),
  });
  assert.deepEqual(plan.blockedReasons, [
    "publication in progress or interrupted: 2.1.0",
  ]);
  assert.equal(
    plan.candidates.length,
    1,
    "dry-run still reports what would be eligible",
  );
});

test("journaled deletion retries idempotently after a partial failure", async () => {
  const version = "1.5.0";
  const currentVersion = "2.0.0";
  const markerKey = deletionMarkerKey(version);
  const state = new Set([
    "latest.txt",
    "manifest.json",
    ...expectedArtifactKeys(version),
  ]);
  let failKey = expectedArtifactKeys(version)[2];
  let transientKey = null;
  let transientFailures = 0;
  const client = {
    async putStateObject(key, body) {
      assert.equal(body.policy, POLICY_CONFIRMATION);
      state.add(key);
    },
    async deleteArtifact(key) {
      if (key === failKey)
        throw new Error("transient outage exhausted retries");
      if (key === transientKey && transientFailures++ === 0)
        throw new Error("retry me");
      state.delete(key); // deleting an already-absent key is deliberately okay
    },
    async deleteStateObject(key) {
      state.delete(key);
    },
  };
  const candidate = {
    version,
    keys: expectedArtifactKeys(version),
    markerKey,
    resume: false,
  };
  await assert.rejects(
    deleteRelease({
      candidate,
      currentVersion,
      client,
      revalidate: async () => {},
      retry: { attempts: 1, sleep: async () => {} },
    }),
    /transient outage/,
  );
  assert.ok(state.has(markerKey), "journal survives a partial delete");
  assert.ok(
    !state.has(expectedArtifactKeys(version)[0]),
    "the first delete landed",
  );
  assert.ok(state.has("latest.txt"));
  assert.ok(state.has("manifest.json"));

  failKey = null;
  transientKey = expectedArtifactKeys(version)[3];
  await deleteRelease({
    candidate: { ...candidate, resume: true },
    currentVersion,
    client,
    revalidate: async () => {},
    retry: { attempts: 2, sleep: async () => {} },
  });
  assert.equal(transientFailures, 2, "a transient delete is retried");
  assert.deepEqual([...state].sort(), ["latest.txt", "manifest.json"]);
});

test("a partial release can resume only from a valid exact-key deletion journal", () => {
  const versions = ["2.0.0", "1.9.0", "1.8.0", "1.7.0", "1.6.0"];
  const version = "1.5.0";
  const marker = deletionMarkerKey(version);
  const objects = [
    ...inventory(...versions),
    object(marker),
    ...releaseObjects(version).slice(2),
  ];
  const journal = {
    policy: POLICY_CONFIRMATION,
    version,
    keys: expectedArtifactKeys(version),
  };
  const resumable = planRetention({
    objects,
    ...pointers("2.0.0"),
    archive: archive(...versions, version),
    deletionJournals: new Map([[version, journal]]),
  });
  assert.equal(resumable.candidates[0].version, version);
  assert.equal(resumable.candidates[0].resume, true);

  const invalid = planRetention({
    objects,
    ...pointers("2.0.0"),
    archive: archive(...versions, version),
    deletionJournals: new Map([
      [version, { ...journal, keys: ["latest.txt"] }],
    ]),
  });
  assert.equal(invalid.candidates.length, 0);
  assert.match(invalid.blockedReasons.join(" "), /invalid deletion journal/);
});

test("the R2 delete boundary rejects pointers, current artifacts, and unmanaged keys", async () => {
  let requests = 0;
  const client = new CloudflareR2Client({
    accountId: "a".repeat(32),
    apiToken: "test-token",
    fetchImpl: async () => {
      requests++;
      return new Response(JSON.stringify({ success: true, result: {} }), {
        status: 200,
      });
    },
  });
  await assert.rejects(
    client.deleteArtifact("latest.txt", "2.0.0"),
    /pointer object/,
  );
  await assert.rejects(
    client.deleteArtifact(expectedArtifactKeys("2.0.0")[0], "2.0.0"),
    /currently advertised/,
  );
  await assert.rejects(
    client.deleteArtifact("notes.txt", "2.0.0"),
    /unmanaged object/,
  );
  assert.equal(requests, 0, "refusals happen before an HTTP request exists");

  await client.deleteArtifact(expectedArtifactKeys("1.0.0")[0], "2.0.0");
  assert.equal(requests, 1);
});
