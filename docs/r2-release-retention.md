# R2 release retention

`comet-native-releases` is the low-latency delivery cache behind
`https://edge.comet.offhand.dev/releases/*`. It is not the permanent archive.
Every published artifact remains attached to its tagged
[GitHub Release](https://docs.github.com/en/repositories/releasing-projects-on-github/about-releases),
which is the archive of record.

## Product policy and rollback guarantee

R2 retains exactly this managed window:

- the version jointly advertised by `latest.txt` and `manifest.json`; and
- the four immediately preceding, complete SemVer releases.

That is five complete release sets and a direct-download rollback guarantee for
current-minus-1 through current-minus-4. The guarantee does not age out during a
quiet period. A release older than that window remains downloadable from its
GitHub Release, but its `edge.comet.offhand.dev/releases/<asset>` URL is no
longer guaranteed.

The policy is count-based because rollback need is about release distance, not
wall-clock time. A raw root-key lifecycle rule is prohibited: it would also age
out mutable `latest.txt` and `manifest.json` after a quiet period.

Four files make one complete release:

```text
comet-<version>-linux-aarch64.tar.gz
comet-<version>-linux-x86_64.tar.gz
comet-<version>-macos-arm64-app.tar.gz
comet-<version>-macos-arm64.dmg
```

## What the automation will and will not delete

[`scripts/release-retention.mjs`](../scripts/release-retention.mjs) lists the
whole bucket and defaults to a dry run. It only generates deletes for the four
exact filenames above. `latest.txt`, `manifest.json`, unrecognized objects, a
version newer than the advertised version, and incomplete release sets never
enter the deletion API.

Before an old complete set becomes eligible, all of these must be true:

1. `latest.txt` and `manifest.json` contain the same valid version.
2. The advertised version has all four R2 objects and all four manifest entries.
3. No `.publishing/<version>.json` marker exists. The release workflow writes
   that marker before its first R2 artifact and removes it only after the public
   pointer read-back succeeds. An interrupted publication therefore blocks all
   deletion.
4. The candidate has all four R2 objects.
5. A non-draft, non-prerelease `v<version>` GitHub Release contains the same four
   assets.
6. The candidate is below the four-release rollback window.

Apply writes `.retention/deleting/<version>.json` before deleting anything. The
marker names a closed, generated four-key set. If a request fails halfway, the
next run resumes that same set; already-absent deletes are harmless. Before
every artifact delete, automation re-reads both pointers, checks for a
publication marker, and verifies the GitHub archive again. The release and
retention workflows also share the `r2-release-mutation` concurrency group, so
the supported publication path cannot move the pointer during cleanup.

Do not move release pointers or upload release artifacts outside
`.github/workflows/release.yml`. An out-of-band writer does not participate in
that concurrency lock.

## Operation and activation

[`.github/workflows/r2-release-retention.yml`](../.github/workflows/r2-release-retention.yml)
runs every Monday. Merging it does **not** enable deletion: while the repository
variable `R2_RELEASE_RETENTION_POLICY` is absent, scheduled runs are audits.
Manual runs also default to `audit`.

1. Run an audit and inspect the release-by-release dispositions:

   ```bash
   gh workflow run r2-release-retention.yml -f mode=audit
   ```

2. After the run identifies the expected current and rollback set, apply once
   with the reviewed phrase:

   ```bash
   gh workflow run r2-release-retention.yml \
     -f mode=apply \
     -f confirm_policy=keep-current-plus-4
   ```

3. To make later scheduled runs enforce the policy, explicitly activate them:

   ```bash
   gh variable set R2_RELEASE_RETENTION_POLICY --body keep-current-plus-4
   ```

   Remove the variable to return the schedule to audit-only mode:

   ```bash
   gh variable delete R2_RELEASE_RETENTION_POLICY
   ```

These are the separate human-confirmed production steps. The implementation
does not alter an R2 lifecycle rule, and merging it alone does not enable
artifact deletion. The workflow uses the existing `CLOUDFLARE_API_TOKEN`; it
reads the account ID from `edge/wrangler.jsonc` and uses `GITHUB_TOKEN` only to
verify archive assets. The object listing and deletes use Cloudflare's documented
[R2 object API](https://developers.cloudflare.com/api/resources/r2/subresources/buckets/subresources/objects),
not a bucket-wide lifecycle.

If a run blocks on `.publishing/<version>.json`, repair or rerun that tag's
release workflow. Do not clear the marker merely to make retention green. If a
`.retention/deleting/<version>.json` marker remains, rerun apply; the deletion
path is idempotent. If that version has since become current or entered the
rollback window, apply stops and the missing assets must be restored from its
GitHub Release before changing the pointer.

## Storage and cost projection

The read-only audit on 2026-08-16 found 66 objects and 1.46 GB created since
2026-08-06. Sixty-four artifacts are exactly 16 four-file release sets, so the
observed mean is about 0.091 GB per release. At that ten-day launch rate:

- no retention adds about 4.38 GB every 30 days and reaches 10 GB after roughly
  another 59 days;
- after a year, the same linear rate would leave about 53 GB stored; and
- the five-release R2 window is about 0.46 GB, plus two pointers and tiny state
  markers. `comet-native-blobs` was another 0.0095 GB at audit time.

Cloudflare currently includes 10 GB-month of Standard R2 storage per month and
charges $0.015 per additional GB-month; deletes are free. On the observed
account, the retained release window plus the blob bucket remains below the
storage allowance, so projected storage cost is $0/month while other R2 usage
does not consume it. Before the shared free allowance, 0.46 GB has a raw list
price of about $0.0069/month (billing-unit rounding can change the invoice).
Without retention, the year-end monthly storage run rate would be roughly
`(53 - 10) × $0.015 = $0.65` under the same assumptions. See Cloudflare's
[current R2 pricing and billing method](https://developers.cloudflare.com/r2/pricing/).
The weekly listing is only a handful of Class A operations against the monthly
one-million-operation allowance, and R2 object deletes are free.

The important bound is operational rather than today's small invoice: the
delivery cache converges near five releases instead of growing with every tag,
while the GitHub archive retains the full history. Protected malformed,
incomplete, or unarchived objects are outside that 0.46 GB projection; they are
reported rather than silently deleted and require an operator to repair their
publication or archive.
