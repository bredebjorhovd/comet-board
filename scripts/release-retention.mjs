#!/usr/bin/env node
/**
 * Retention for the immutable release artifacts in comet-native-releases.
 *
 * With no arguments this is a read-only audit. `--apply` is deliberately
 * awkward: the reviewed policy phrase must be supplied, both mutable pointers
 * must agree, the current release must be complete, GitHub Releases must hold
 * every candidate asset, and no release publication may be in flight.
 *
 * The release workflow also uses the two publication-marker commands. They
 * make an interrupted upload visible to retention instead of asking it to
 * infer intent from a temporarily incomplete set of root objects.
 */

import { pathToFileURL } from "node:url";

export const RELEASE_BUCKET = "comet-native-releases";
export const ARCHIVE_REPOSITORY = "bredebjorhovd/comet-board";
export const RETAIN_COMPLETE_RELEASES = 5;
export const ROLLBACK_RELEASES = RETAIN_COMPLETE_RELEASES - 1;
export const POLICY_CONFIRMATION = "keep-current-plus-4";
export const POINTER_KEYS = new Set(["latest.txt", "manifest.json"]);
export const PUBLICATION_PREFIX = ".publishing/";
export const DELETION_PREFIX = ".retention/deleting/";

const ARTIFACT_SUFFIXES = [
  "-linux-aarch64.tar.gz",
  "-linux-x86_64.tar.gz",
  "-macos-arm64-app.tar.gz",
  "-macos-arm64.dmg",
];

const VERSION_RE =
  /^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/;

const requireVersion = (value, context = "version") => {
  const version = String(value ?? "")
    .trim()
    .replace(/^v/, "");
  if (!VERSION_RE.test(version))
    throw new Error(`${context} is not a supported SemVer: ${value}`);
  return version;
};

export const expectedArtifactKeys = (versionValue) => {
  const version = requireVersion(versionValue);
  return ARTIFACT_SUFFIXES.map((suffix) => `comet-${version}${suffix}`);
};

export const publicationMarkerKey = (versionValue) =>
  `${PUBLICATION_PREFIX}${requireVersion(versionValue)}.json`;

export const deletionMarkerKey = (versionValue) =>
  `${DELETION_PREFIX}${requireVersion(versionValue)}.json`;

export const parseArtifactKey = (key) => {
  if (!key.startsWith("comet-")) return null;
  for (const suffix of ARTIFACT_SUFFIXES) {
    if (!key.endsWith(suffix)) continue;
    const version = key.slice("comet-".length, -suffix.length);
    if (!VERSION_RE.test(version)) return null;
    return { version, key };
  }
  return null;
};

const parseStateVersion = (key, prefix) => {
  if (!key.startsWith(prefix) || !key.endsWith(".json")) return null;
  const version = key.slice(prefix.length, -".json".length);
  return VERSION_RE.test(version) ? version : null;
};

const parseSemver = (value) => {
  const version = requireVersion(value);
  const withoutBuild = version.includes("+")
    ? version.slice(0, version.indexOf("+"))
    : version;
  const dash = withoutBuild.indexOf("-");
  const core = dash === -1 ? withoutBuild : withoutBuild.slice(0, dash);
  const prerelease = dash === -1 ? undefined : withoutBuild.slice(dash + 1);
  return {
    version,
    core: core.split(".").map(Number),
    prerelease: prerelease === undefined ? null : prerelease.split("."),
  };
};

/** SemVer precedence, sufficient for release ordering and deterministic ties. */
export const compareVersions = (leftValue, rightValue) => {
  const left = parseSemver(leftValue);
  const right = parseSemver(rightValue);
  for (let index = 0; index < 3; index++) {
    if (left.core[index] !== right.core[index])
      return left.core[index] - right.core[index];
  }
  if (left.prerelease === null && right.prerelease !== null) return 1;
  if (left.prerelease !== null && right.prerelease === null) return -1;
  if (left.prerelease === null)
    return left.version.localeCompare(right.version);
  const length = Math.max(left.prerelease.length, right.prerelease.length);
  for (let index = 0; index < length; index++) {
    const l = left.prerelease[index];
    const r = right.prerelease[index];
    if (l === undefined) return -1;
    if (r === undefined) return 1;
    if (l === r) continue;
    const lNumeric = /^\d+$/.test(l);
    const rNumeric = /^\d+$/.test(r);
    if (lNumeric && rNumeric) return Number(l) - Number(r);
    if (lNumeric !== rNumeric) return lNumeric ? -1 : 1;
    return l.localeCompare(r);
  }
  // Build metadata has equal SemVer precedence. Use the full normalized text
  // only as a stable inventory tie-break so list-page order cannot affect the
  // retention boundary.
  return left.version.localeCompare(right.version);
};

const pointerVersion = ({ latestText, manifest, objectKeys }) => {
  const latest = requireVersion(latestText, "latest.txt");
  const manifestVersion = requireVersion(
    manifest?.version,
    "manifest.json version",
  );
  if (latest !== manifestVersion) {
    throw new Error(
      `pointer disagreement: latest.txt=${latest}, manifest.json=${manifestVersion}`,
    );
  }
  if (
    !manifest?.files ||
    typeof manifest.files !== "object" ||
    Array.isArray(manifest.files)
  ) {
    throw new Error("manifest.json has no files object");
  }
  const missingFromBucket = expectedArtifactKeys(latest).filter(
    (key) => !objectKeys.has(key),
  );
  const missingFromManifest = expectedArtifactKeys(latest).filter(
    (key) => !(key in manifest.files),
  );
  const invalidChecksums = expectedArtifactKeys(latest).filter(
    (key) =>
      key in manifest.files &&
      !/^[a-f0-9]{64}$/.test(manifest.files[key]?.sha256 ?? ""),
  );
  if (
    missingFromBucket.length > 0 ||
    missingFromManifest.length > 0 ||
    invalidChecksums.length > 0
  ) {
    throw new Error(
      `advertised release ${latest} is incomplete` +
        `${missingFromBucket.length ? `; missing from R2: ${missingFromBucket.join(", ")}` : ""}` +
        `${missingFromManifest.length ? `; missing from manifest: ${missingFromManifest.join(", ")}` : ""}` +
        `${invalidChecksums.length ? `; invalid sha256: ${invalidChecksums.join(", ")}` : ""}`,
    );
  }
  return latest;
};

const archiveHasRelease = (archive, version) => {
  const release = archive.get(version);
  if (!release || release.draft || release.prerelease) return false;
  const assets = new Set(release.assets);
  return expectedArtifactKeys(version).every((key) => assets.has(key));
};

const validDeletionJournal = (journal, version) => {
  if (
    !journal ||
    journal.policy !== POLICY_CONFIRMATION ||
    journal.version !== version
  )
    return false;
  const expected = expectedArtifactKeys(version);
  return (
    Array.isArray(journal.keys) &&
    journal.keys.length === expected.length &&
    journal.keys.every((key, index) => key === expected[index])
  );
};

/**
 * Pure retention decision. Object age is intentionally absent: a quiet period
 * keeps all rollback releases forever, while a busy period never grows the
 * complete-release window beyond the reviewed count.
 */
export const planRetention = ({
  objects,
  latestText,
  manifest,
  archive = new Map(),
  deletionJournals = new Map(),
}) => {
  const objectKeys = new Set(objects.map((object) => object.key));
  if (!objectKeys.has("latest.txt") || !objectKeys.has("manifest.json")) {
    throw new Error("release bucket is missing latest.txt or manifest.json");
  }
  const currentVersion = pointerVersion({ latestText, manifest, objectKeys });
  const releases = new Map();
  const publishingVersions = new Set();
  const deletingVersions = new Set();
  const invalidStateReasons = [];
  const unknownObjects = [];

  const releaseFor = (version) => {
    if (!releases.has(version)) releases.set(version, { version, keys: [] });
    return releases.get(version);
  };

  for (const object of objects) {
    const artifact = parseArtifactKey(object.key);
    if (artifact) {
      releaseFor(artifact.version).keys.push(object.key);
      continue;
    }
    if (object.key.startsWith(PUBLICATION_PREFIX)) {
      const publishing = parseStateVersion(object.key, PUBLICATION_PREFIX);
      if (publishing) {
        publishingVersions.add(publishing);
        releaseFor(publishing);
      } else {
        invalidStateReasons.push(`malformed publication marker: ${object.key}`);
        unknownObjects.push(object.key);
      }
      continue;
    }
    if (object.key.startsWith(DELETION_PREFIX)) {
      const deleting = parseStateVersion(object.key, DELETION_PREFIX);
      if (deleting) {
        deletingVersions.add(deleting);
        releaseFor(deleting);
      } else {
        invalidStateReasons.push(`malformed deletion marker: ${object.key}`);
        unknownObjects.push(object.key);
      }
      continue;
    }
    if (!POINTER_KEYS.has(object.key)) unknownObjects.push(object.key);
  }

  const sorted = [...releases.values()].sort((a, b) =>
    compareVersions(b.version, a.version),
  );
  for (const release of sorted) {
    const expected = expectedArtifactKeys(release.version);
    const keys = new Set(release.keys);
    release.keys = expected.filter((key) => keys.has(key));
    release.missing = expected.filter((key) => !keys.has(key));
    release.complete = release.missing.length === 0;
    release.publishing = publishingVersions.has(release.version);
    release.deleting = deletingVersions.has(release.version);
    release.deletionJournalValid =
      release.deleting &&
      validDeletionJournal(
        deletionJournals.get(release.version),
        release.version,
      );
  }

  // A deletion marker stands for a release that was complete when deletion
  // began. Count it in the window on retries; otherwise the partial first pass
  // would shift the boundary and make the retry's decision differ.
  const priorComplete = sorted
    .filter(
      (release) =>
        compareVersions(release.version, currentVersion) < 0 &&
        (release.complete || release.deletionJournalValid),
    )
    .slice(0, ROLLBACK_RELEASES)
    .map((release) => release.version);
  const rollback = new Set(priorComplete);
  const blockedReasons = [
    ...invalidStateReasons,
    ...[...publishingVersions]
      .sort(compareVersions)
      .map((version) => `publication in progress or interrupted: ${version}`),
  ];
  for (const release of sorted) {
    if (release.deleting && !release.deletionJournalValid) {
      blockedReasons.push(`invalid deletion journal: ${release.version}`);
    }
  }
  const candidates = [];

  for (const release of sorted) {
    const relation = compareVersions(release.version, currentVersion);
    if (release.version === currentVersion) {
      release.disposition = "keep-current";
      release.reason = "advertised by latest.txt and manifest.json";
    } else if (relation > 0) {
      release.disposition = "protect-newer";
      release.reason =
        "newer than the advertised version; may be a deliberate rollback";
    } else if (rollback.has(release.version)) {
      release.disposition = "keep-rollback";
      release.reason = `one of ${ROLLBACK_RELEASES} supported rollback releases`;
    } else if (release.publishing) {
      release.disposition = "protect-publishing";
      release.reason = "publication marker present";
    } else if (release.deleting && !release.deletionJournalValid) {
      release.disposition = "protect-journal";
      release.reason =
        "deletion marker does not contain the reviewed exact-key journal";
    } else if (!release.complete && !release.deleting) {
      release.disposition = "protect-incomplete";
      release.reason = `incomplete publication; missing ${release.missing.join(", ")}`;
    } else if (!archiveHasRelease(archive, release.version)) {
      release.disposition = "protect-unarchived";
      release.reason =
        "GitHub release is missing, draft/prerelease, or lacks an expected asset";
    } else {
      release.disposition = release.deletionJournalValid
        ? "resume-delete"
        : "delete";
      release.reason = release.deletionJournalValid
        ? "journaled deletion is safe to retry"
        : `older than current plus ${ROLLBACK_RELEASES}; complete GitHub archive verified`;
      candidates.push({
        version: release.version,
        keys: expectedArtifactKeys(release.version),
        markerKey: deletionMarkerKey(release.version),
        resume: release.deletionJournalValid,
      });
    }
  }

  const totalBytes = objects.reduce(
    (sum, object) => sum + Number(object.size ?? 0),
    0,
  );
  return {
    currentVersion,
    rollbackVersions: priorComplete,
    releases: sorted,
    candidates,
    blockedReasons,
    unknownObjects: unknownObjects.sort(),
    objectCount: objects.length,
    totalBytes,
  };
};

const delay = (milliseconds) =>
  new Promise((resolve) => setTimeout(resolve, milliseconds));

export const retryOperation = async (
  operation,
  { attempts = 5, sleep = delay, label = "R2 operation" } = {},
) => {
  let lastError;
  for (let attempt = 1; attempt <= attempts; attempt++) {
    try {
      return await operation();
    } catch (error) {
      lastError = error;
      if (error?.retryable === false || attempt === attempts) break;
      await sleep(attempt * attempt * 250);
    }
  }
  throw new Error(
    `${label} failed after ${attempts} attempt(s): ${lastError?.message ?? lastError}`,
    {
      cause: lastError,
    },
  );
};

const responseMessage = async (response) => {
  const text = await response.text();
  if (!text) return `${response.status} ${response.statusText}`;
  try {
    const body = JSON.parse(text);
    const errors = body?.errors?.map((error) => error.message).filter(Boolean);
    return errors?.length ? errors.join("; ") : text;
  } catch {
    return text;
  }
};

const requestError = async (response) => {
  const error = new Error(
    `${response.status} ${await responseMessage(response)}`,
  );
  error.retryable = response.status === 429 || response.status >= 500;
  return error;
};

const encodeKey = (key) => key.split("/").map(encodeURIComponent).join("/");

const isManagedStateKey = (key) =>
  parseStateVersion(key, PUBLICATION_PREFIX) !== null ||
  parseStateVersion(key, DELETION_PREFIX) !== null;

export class CloudflareR2Client {
  constructor({ accountId, apiToken, fetchImpl = fetch }) {
    if (!/^[a-f0-9]{32}$/.test(accountId ?? ""))
      throw new Error("CLOUDFLARE_ACCOUNT_ID is missing or invalid");
    if (!apiToken) throw new Error("CLOUDFLARE_API_TOKEN is required");
    this.base = `https://api.cloudflare.com/client/v4/accounts/${accountId}/r2/buckets/${RELEASE_BUCKET}`;
    this.fetch = fetchImpl;
    this.headers = { authorization: `Bearer ${apiToken}` };
  }

  async listObjects({ prefix } = {}) {
    const objects = [];
    let cursor;
    do {
      const url = new URL(`${this.base}/objects`);
      url.searchParams.set("per_page", "1000");
      if (prefix) url.searchParams.set("prefix", prefix);
      if (cursor) url.searchParams.set("cursor", cursor);
      const response = await retryOperation(
        () =>
          this.fetch(url, { headers: this.headers }).then(async (result) => {
            if (!result.ok) throw await requestError(result);
            return result;
          }),
        { label: `list ${RELEASE_BUCKET}` },
      );
      const body = await response.json();
      if (!body.success || !Array.isArray(body.result))
        throw new Error("Cloudflare returned an invalid object listing");
      objects.push(...body.result);
      cursor = body.result_info?.is_truncated
        ? body.result_info?.cursor
        : undefined;
      if (body.result_info?.is_truncated && !cursor)
        throw new Error("truncated R2 listing has no cursor");
    } while (cursor);
    return objects;
  }

  async getObject(key) {
    const response = await retryOperation(
      () =>
        this.fetch(`${this.base}/objects/${encodeKey(key)}`, {
          headers: this.headers,
        }).then(async (result) => {
          if (!result.ok) throw await requestError(result);
          return result;
        }),
      { label: `get ${key}` },
    );
    return response.text();
  }

  async putStateObject(key, value) {
    if (!isManagedStateKey(key)) {
      throw new Error(`refusing to write non-state object: ${key}`);
    }
    const result = await this.fetch(`${this.base}/objects/${encodeKey(key)}`, {
      method: "PUT",
      headers: { ...this.headers, "content-type": "application/octet-stream" },
      body: JSON.stringify(value),
    });
    if (!result.ok) throw await requestError(result);
  }

  async deleteArtifact(key, currentVersion) {
    if (POINTER_KEYS.has(key))
      throw new Error(`refusing to delete pointer object: ${key}`);
    const artifact = parseArtifactKey(key);
    if (!artifact || !expectedArtifactKeys(artifact.version).includes(key)) {
      throw new Error(`refusing to delete unmanaged object: ${key}`);
    }
    if (artifact.version === currentVersion) {
      throw new Error(
        `refusing to delete currently advertised release ${currentVersion}`,
      );
    }
    await this.#deleteObject(key);
  }

  async deleteStateObject(key) {
    if (!isManagedStateKey(key)) {
      throw new Error(
        `refusing to delete non-state object through state path: ${key}`,
      );
    }
    await this.#deleteObject(key);
  }

  async #deleteObject(key) {
    const result = await this.fetch(`${this.base}/objects/${encodeKey(key)}`, {
      method: "DELETE",
      headers: this.headers,
    });
    if (result.status === 404) return;
    if (!result.ok) throw await requestError(result);
  }
}

class GitHubArchiveClient {
  constructor({ repository = ARCHIVE_REPOSITORY, token, fetchImpl = fetch }) {
    this.base = `https://api.github.com/repos/${repository}`;
    this.fetch = fetchImpl;
    this.headers = {
      accept: "application/vnd.github+json",
      "x-github-api-version": "2022-11-28",
      "user-agent": "comet-release-retention",
      ...(token ? { authorization: `Bearer ${token}` } : {}),
    };
  }

  async release(version) {
    const response = await retryOperation(
      () =>
        this.fetch(
          `${this.base}/releases/tags/${encodeURIComponent(`v${version}`)}`,
          {
            headers: this.headers,
          },
        ).then(async (result) => {
          if (result.status === 404) return result;
          if (!result.ok) throw await requestError(result);
          return result;
        }),
      { label: `read GitHub release v${version}` },
    );
    if (response.status === 404) return null;
    const body = await response.json();
    return {
      draft: Boolean(body.draft),
      prerelease: Boolean(body.prerelease),
      assets: Array.isArray(body.assets)
        ? body.assets.map((asset) => asset.name)
        : [],
    };
  }
}

const loadPointers = async (client) => {
  const [latestText, manifestText] = await Promise.all([
    client.getObject("latest.txt"),
    client.getObject("manifest.json"),
  ]);
  let manifest;
  try {
    manifest = JSON.parse(manifestText);
  } catch (error) {
    throw new Error(`manifest.json is not valid JSON: ${error.message}`);
  }
  return { latestText, manifest };
};

const candidateVersions = ({
  objects,
  latestText,
  manifest,
  deletionJournals,
}) => {
  const optimisticArchive = new Map();
  for (const object of objects) {
    const artifact = parseArtifactKey(object.key);
    const deleting = parseStateVersion(object.key, DELETION_PREFIX);
    const version = artifact?.version ?? deleting;
    if (version) {
      optimisticArchive.set(version, {
        draft: false,
        prerelease: false,
        assets: expectedArtifactKeys(version),
      });
    }
  }
  return planRetention({
    objects,
    latestText,
    manifest,
    archive: optimisticArchive,
    deletionJournals,
  }).candidates.map((candidate) => candidate.version);
};

const buildVerifiedPlan = async ({ r2, github }) => {
  const [objects, pointers] = await Promise.all([
    r2.listObjects(),
    loadPointers(r2),
  ]);
  const deletionJournals = new Map();
  await Promise.all(
    objects.map(async (object) => {
      const version = parseStateVersion(object.key, DELETION_PREFIX);
      if (!version) return;
      const text = await r2.getObject(object.key);
      try {
        deletionJournals.set(version, JSON.parse(text));
      } catch {
        deletionJournals.set(version, null);
      }
    }),
  );
  const versions = candidateVersions({
    objects,
    ...pointers,
    deletionJournals,
  });
  const archive = new Map();
  await Promise.all(
    versions.map(async (version) => {
      const release = await github.release(version);
      if (release) archive.set(version, release);
    }),
  );
  return planRetention({ objects, ...pointers, archive, deletionJournals });
};

const revalidateBeforeDelete = async ({
  r2,
  github,
  plannedCurrent,
  deletingVersion,
}) => {
  const [pointers, publishing, archivedRelease] = await Promise.all([
    loadPointers(r2),
    r2.listObjects({ prefix: PUBLICATION_PREFIX }),
    github.release(deletingVersion),
  ]);
  const current = requireVersion(pointers.latestText, "latest.txt");
  const manifestVersion = requireVersion(
    pointers.manifest?.version,
    "manifest.json version",
  );
  if (current !== plannedCurrent || manifestVersion !== plannedCurrent) {
    throw new Error(
      `pointers changed during retention: planned=${plannedCurrent}, latest=${current}, manifest=${manifestVersion}`,
    );
  }
  if (deletingVersion === current)
    throw new Error(
      `refusing to delete currently advertised release ${current}`,
    );
  if (publishing.length > 0)
    throw new Error("publication began during retention; refusing to delete");
  if (
    !archiveHasRelease(
      new Map([[deletingVersion, archivedRelease]]),
      deletingVersion,
    )
  ) {
    throw new Error(
      `GitHub archive changed during retention; refusing to delete ${deletingVersion}`,
    );
  }
};

/** Journal first, then delete a closed, generated set. Safe to call again. */
export const deleteRelease = async ({
  candidate,
  currentVersion,
  client,
  revalidate,
  retry = {},
}) => {
  if (candidate.version === currentVersion) {
    throw new Error(
      `refusing to delete currently advertised release ${currentVersion}`,
    );
  }
  const expected = expectedArtifactKeys(candidate.version);
  if (
    candidate.keys.length !== expected.length ||
    candidate.keys.some((key, index) => key !== expected[index])
  ) {
    throw new Error(
      `candidate ${candidate.version} does not contain the exact managed artifact set`,
    );
  }
  await revalidate(candidate.version);
  if (!candidate.resume) {
    await retryOperation(
      () =>
        client.putStateObject(candidate.markerKey, {
          policy: POLICY_CONFIRMATION,
          version: candidate.version,
          keys: expected,
        }),
      { ...retry, label: `journal deletion of ${candidate.version}` },
    );
  }
  for (const key of expected) {
    await revalidate(candidate.version);
    await retryOperation(() => client.deleteArtifact(key, currentVersion), {
      ...retry,
      label: `delete ${key}`,
    });
  }
  await revalidate(candidate.version);
  await retryOperation(() => client.deleteStateObject(candidate.markerKey), {
    ...retry,
    label: `clear deletion journal for ${candidate.version}`,
  });
};

const gibibytes = (bytes) => bytes / 1024 ** 3;

export const renderPlan = (plan, { apply = false } = {}) => {
  const lines = [
    `R2 release retention: ${apply ? "APPLY" : "DRY RUN"}`,
    `Policy: keep advertised ${plan.currentVersion} plus ${ROLLBACK_RELEASES} rollback release(s); GitHub Releases is the permanent archive.`,
    `Inventory: ${plan.objectCount} object(s), ${gibibytes(plan.totalBytes).toFixed(2)} GiB.`,
    `Rollback set: ${plan.rollbackVersions.length ? plan.rollbackVersions.join(", ") : "none"}.`,
  ];
  for (const release of plan.releases) {
    lines.push(
      `${release.disposition.padEnd(19)} ${release.version.padEnd(18)} ${release.reason}`,
    );
  }
  if (plan.unknownObjects.length) {
    lines.push(
      `Protected unmanaged object(s): ${plan.unknownObjects.join(", ")}`,
    );
  }
  if (plan.blockedReasons.length) {
    lines.push(`BLOCKED: ${plan.blockedReasons.join("; ")}`);
  }
  const candidateObjects = plan.candidates.length * ARTIFACT_SUFFIXES.length;
  lines.push(
    `${plan.candidates.length} release(s) / ${candidateObjects} managed object(s) eligible for deletion.`,
  );
  if (!apply) lines.push("Dry run only: no object was changed.");
  return lines.join("\n");
};

const configFromEnvironment = () => ({
  accountId: process.env.CLOUDFLARE_ACCOUNT_ID,
  apiToken: process.env.CLOUDFLARE_API_TOKEN,
});

const publicationCommand = async (action, versionValue) => {
  const version = requireVersion(versionValue);
  const r2 = new CloudflareR2Client(configFromEnvironment());
  const key = publicationMarkerKey(version);
  if (action === "publication-start") {
    await retryOperation(
      () =>
        r2.putStateObject(key, {
          version,
          runId: process.env.GITHUB_RUN_ID ?? null,
          startedAt: new Date().toISOString(),
        }),
      { label: `write publication marker for ${version}` },
    );
    console.log(`publication marker written: ${key}`);
    return;
  }
  await retryOperation(() => r2.deleteStateObject(key), {
    label: `clear publication marker for ${version}`,
  });
  console.log(`publication marker cleared: ${key}`);
};

const retentionCommand = async (argv) => {
  const applyIndex = argv.indexOf("--apply");
  const apply = applyIndex !== -1;
  const confirmationIndex = argv.indexOf("--confirm-policy");
  const confirmation =
    confirmationIndex === -1 ? undefined : argv[confirmationIndex + 1];
  const known = new Set(["--apply", "--confirm-policy", confirmation]);
  const unknown = argv.filter(
    (argument) => argument !== undefined && !known.has(argument),
  );
  if (unknown.length)
    throw new Error(`unknown argument(s): ${unknown.join(", ")}`);
  if (apply && confirmation !== POLICY_CONFIRMATION) {
    throw new Error(`--apply requires --confirm-policy ${POLICY_CONFIRMATION}`);
  }
  const r2 = new CloudflareR2Client(configFromEnvironment());
  const github = new GitHubArchiveClient({ token: process.env.GITHUB_TOKEN });
  const plan = await buildVerifiedPlan({ r2, github });
  console.log(renderPlan(plan, { apply }));
  if (!apply) return;
  if (plan.blockedReasons.length) {
    throw new Error(
      `retention is blocked by safety condition(s): ${plan.blockedReasons.join("; ")}`,
    );
  }
  for (const candidate of plan.candidates) {
    await deleteRelease({
      candidate,
      currentVersion: plan.currentVersion,
      client: r2,
      revalidate: (version) =>
        revalidateBeforeDelete({
          r2,
          github,
          plannedCurrent: plan.currentVersion,
          deletingVersion: version,
        }),
    });
    console.log(`deleted archived R2 release ${candidate.version}`);
  }
};

const main = async () => {
  const [command, version] = process.argv.slice(2);
  if (command === "publication-start" || command === "publication-finish") {
    if (!version || process.argv.length !== 4) {
      throw new Error(`usage: release-retention.mjs ${command} <version>`);
    }
    await publicationCommand(command, version);
    return;
  }
  await retentionCommand(process.argv.slice(2));
};

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(process.argv[1]).href
) {
  main().catch((error) => {
    console.error(`release-retention: ${error.message}`);
    process.exitCode = 1;
  });
}
