# A release is not done until the edge says so — **done** (gh#197)

Cutting v0.3.5 on 2026-08-09, the three build jobs passed and all four artifacts
reached the GitHub release. The `publish` job then died uploading to R2 on a
**502 from `api.cloudflare.com`** — wrangler's own request, refused by the API
gateway, nothing to do with the payload. What that left: `gh release view
v0.3.5` showing four assets and a tag, and `edge…/releases/latest.txt` still
reading **`0.3.4`**. `install.sh` reads `latest.txt`, so a box upgrading in that
window is handed the *previous* release and reports success. This one would have
been actively harmful — the upgrade existed to ship §gh#156's `comet-board`, and the
plan was to `rm -f` the hand-placed one first, which would have left that machine
with no board CLI at all. It was caught because somebody checked the version by
hand.

- **Retries, because a 502 from a gateway is the textbook transient.**
  `scripts/r2-put.sh` wraps every put: five attempts, 5/20/45/80s backoff, and
  `::error::` on the last one. A put is idempotent — same key, same bytes off the
  same disk — so retrying is free. `gh run rerun --failed` fixed the real one on
  the first try, which is the definition of something the pipeline should have
  swallowed itself.
- **The pointer moves last, and only onto artifacts that answer.** The order was
  already right; it is now enforced rather than merely written down. Artifacts,
  then `manifest.json`, then a step that fetches every one of them back **through
  the public host** (`?v=$GITHUB_RUN_ID`, because the mutable objects carry
  `max-age=60` and a cached copy of the previous manifest would be reassurance
  about the wrong release) — and only then `latest.txt`, which is read back the
  same way. The inverse failure, a pointer ahead of its artifacts, is worse than
  what happened: a lagging pointer is a box that does not upgrade, a leading one
  is every box 404ing mid-upgrade.
- **The release is a prerelease until the edge is serving it.** This is the part
  that was silent: the run went red, but the release page looked finished, so
  everything that reads a release from the tag alone — a human included — got the
  wrong answer. `softprops/action-gh-release` now publishes with `prerelease:
  true`, and a `promote` step clears it only after the verification passes. The
  "Latest" badge therefore stays on the version the edge is actually handing out.
  On failure, `scripts/release-notice.sh add` writes the reason into the release
  body between HTML-comment markers; `clear` on the next successful run removes
  exactly that block, so the notice neither stacks nor outlives the problem.
- **`doctor` can see it from the box.** The new `release` check reports what the
  engine's release checker last saw at the edge beside what this box runs, and
  when it last looked — read off the `UpdateStatus` stream rather than fetched,
  so doctor cannot form a second opinion the updater would disagree with. Behind
  is printed, never failed (every box is behind for the window after a release).
  Red in one state: the edge serving something **older** than what runs here —
  not a box anybody forgot to upgrade but the install surface pointing backwards,
  which is what a half-landed publish looks like from a machine. A `latest.txt`
  that will not parse fails too: the updater ignores garbage in silence, so
  nothing else on the box would ever mention it.
