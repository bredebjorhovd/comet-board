#!/bin/sh
# r2-put.sh <key> <file> — put one object into the release bucket, with retries.
#
#   scripts/r2-put.sh comet-0.3.6-linux-x86_64.tar.gz artifacts/comet-…tar.gz
#
# Used by .github/workflows/release.yml for every object it publishes. It exists
# because of gh#197: cutting v0.3.5, one `wrangler r2 object put` came back with
# a 502 from api.cloudflare.com — the API gateway's own HTML error page,
# refusing wrangler's request before it reached the bucket. Nothing was
# half-written and `gh run rerun --failed` succeeded on the first try, which is
# the definition of a transient the pipeline should absorb rather than end a
# publish over.
#
# What ending it cost: the GitHub release already had all four assets, so it
# looked finished, while `latest.txt` still named the previous version — and
# `install.sh` reads `latest.txt`. A box upgrading in that window is handed the
# older release and reports success.
#
# Retries are safe here precisely because a put is idempotent: same key, same
# bytes, and the last writer wins with the content we already have on disk.
set -eu

BUCKET="${R2_BUCKET:-comet-native-releases}"
ATTEMPTS="${R2_PUT_ATTEMPTS:-5}"

key="${1:?usage: r2-put.sh <key> <file>}"
file="${2:?usage: r2-put.sh <key> <file>}"
[ -f "$file" ] || { echo "r2-put: $file does not exist" >&2; exit 1; }

attempt=1
while :; do
  if npx wrangler@4 r2 object put "$BUCKET/$key" --file "$file" --remote; then
    exit 0
  fi
  if [ "$attempt" -ge "$ATTEMPTS" ]; then
    # `::error::` rather than a bare message: this is the line that has to be
    # findable in a red run, and the run is the only place the failure shows.
    echo "::error::R2 put failed $ATTEMPTS times for $key — the edge is still serving whatever it was before this run." >&2
    exit 1
  fi
  # 5s, 20s, 45s, 80s. Long enough for a gateway to stop 502ing, short enough
  # that a genuinely broken credential still fails the job inside three minutes.
  delay=$((attempt * attempt * 5))
  echo "::warning::R2 put failed for $key (attempt $attempt of $ATTEMPTS) — retrying in ${delay}s" >&2
  sleep "$delay"
  attempt=$((attempt + 1))
done
