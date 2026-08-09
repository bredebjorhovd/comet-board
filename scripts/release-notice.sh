#!/bin/sh
# release-notice.sh add|clear <tag> <edge-host> — put the "this release did not
# finish publishing" warning on a GitHub release, or take it off again.
#
# gh#197: publishing is two halves — assets onto the GitHub release, artifacts
# into the R2 bucket the edge serves — and the second half can fail on its own.
# When it did (v0.3.5), the release page showed all four assets and the tag
# existed, so everything that reads a release as "done" from the tag alone got
# the wrong answer: `latest.txt` still named the previous version, and a box
# upgrading in that window was handed 0.3.4 and reported success.
#
# The release workflow marks the release a prerelease up front and promotes it
# only once the edge is verified. This adds the *why* in the one place somebody
# looking at the release will actually meet it.
#
# The notice is delimited by HTML comment markers so `clear` can remove exactly
# it and nothing else — a body edited by hand in between keeps its edits, and a
# failure followed by another failure does not stack two copies.
set -eu

MARK_OPEN='<!-- publish-incomplete -->'
MARK_CLOSE='<!-- /publish-incomplete -->'

action="${1:?usage: release-notice.sh add|clear <tag> <edge-host>}"
tag="${2:?usage: release-notice.sh add|clear <tag> <edge-host>}"
host="${3-}"

# Everything outside the marked block, in order.
strip_notice() {
  # `opened`/`closed` rather than the obvious names: `close` is an awk builtin
  # and using it as a variable is a syntax error, not a shadowing warning.
  awk -v opened="$MARK_OPEN" -v closed="$MARK_CLOSE" '
    $0 == opened { skip = 1 }
    !skip        { print }
    $0 == closed { skip = 0 }
  '
}

body=$(gh release view "$tag" --json body -q .body)
# `sed '/./,$!d'` drops the leading blank lines the removed block leaves behind,
# so a release that has been warned and then promoted reads exactly like one
# that never was.
rest=$(printf '%s\n' "$body" | strip_notice | sed -e '/./,$!d')

case "$action" in
  clear)
    # Nothing to do is the common case: only a failed run ever writes one.
    [ "$body" = "$rest" ] && exit 0
    printf '%s\n' "$rest" | gh release edit "$tag" --notes-file -
    echo "release-notice: cleared the publish warning on $tag"
    ;;
  add)
    [ -n "$host" ] || { echo "release-notice: add needs the edge host" >&2; exit 1; }
    {
      echo "$MARK_OPEN"
      echo "> [!WARNING]"
      echo "> **This release is not installable yet — publishing did not finish.**"
      echo "> The assets below are complete, but \`https://$host/releases/latest.txt\`"
      echo "> was not moved onto this version, so \`install.sh\` and \`comet update\`"
      echo "> still hand out the previous release and report success."
      echo ">"
      echo "> Re-run the failed job (\`gh run rerun --failed\`), then confirm with"
      echo "> \`curl -fsSL https://$host/releases/latest.txt\`. This notice and the"
      echo "> pre-release flag clear themselves on a run that finishes."
      echo "$MARK_CLOSE"
      echo ""
      printf '%s\n' "$rest"
    } | gh release edit "$tag" --notes-file -
    echo "release-notice: warned on $tag that the edge was not updated"
    ;;
  *)
    echo "release-notice: unknown action '$action' (add|clear)" >&2
    exit 1
    ;;
esac
