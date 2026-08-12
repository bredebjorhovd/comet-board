# An undispatched pull request is still reviewable — **done** (gh#344)

Raised by Brede on 2026-08-11, and not for the first time: an agent asked
*"which of these is the best fit to start with?"*, answered by doing the work in
its own chat rather than dispatching it, and the pull request it produced was
never reviewable.

The **row** was never the problem. A pull request whose head is not an attempt
branch is upserted as its own `gh!<n>` row and polled like any other — verified
live: `gh!191  done  fix: restore approval lifecycle and CI`
(`codex/restore-green-main`, on `bredebjorhovd/itsm-agent`, no dispatch behind
it).

What was missing was every part of **review**. `authoring_attempt`
(`crates/board/src/review.rs`) finds the attempt whose branch matches the pull
request's head; attempts exist only from dispatch. So an undispatched pull
request had no authoring attempt, therefore no chat to deliver a verdict into,
no claims because nothing was ever told the contract, and no checkout to
assemble a diff from. `SyncEngine::review` answered `has no attempts to review`
and the row travelled `review` → `done` unread. On a board whose proposition is
*"the screen that makes merging cheaper"* (gh#234), the work that most needs
review — work nobody planned, done outside the process — got none.

This is not gh#340. That one is *why the agent did not dispatch*. This one is:
**when it does not, the result must still be reviewable.** Humans open pull
requests too, so this failure mode does not get designed away.

### The attempt is an enrichment, not a requirement

Landed in `crates/board/src/claims.rs` (`pull_request_review`,
`AttemptReview::undispatched`, `NO_ATTEMPT`, `DiffSource::PullRequest`),
`SyncEngine::review`/`pull_request_review` in `crates/board/src/sync.rs`,
`Github::pull_files` in `crates/board/src/sources/github.rs`, `reviewable` in
`crates/proto/src/view/board.rs`, and the three surfaces.

- **The diff is GitHub's.** A pushed branch is on GitHub whether or not this box
  has a checkout of it, so the changed set comes from
  `GET /repos/{repo}/pulls/{n}/files` — path, status, additions, deletions —
  translated into git's own status letters, because the same column is drawn
  beside checkout-read rows and two spellings of one fact read as two facts.
  Asked only when a review is opened on a row with no attempt; never on the
  poll, where it would be a call per open pull request per cycle for a number
  nobody is looking at. Truncation at GitHub's 3000-file cap is an error rather
  than a short list, because a remainder computed against a partial diff says
  "accounted for" about files it never saw.
- **The remainder degrades honestly.** No claims means every changed file is
  unaccounted for, which is *true*. It is not an error state and it is not a
  clean screen.
- **Nobody is blamed for a message nobody sent.** "This attempt never answered
  the claim contract" is a fact about an agent. Said about a pull request no
  agent was ever given, it accuses somebody of ignoring an ask that was never
  made — so the finding reads "nothing dispatched this pull request, so no
  claims were ever made and nothing accounts for its N changed files", in one
  place (`AttemptReview::findings`) that the CLI, the desktop and the phone all
  read.
- **What did not happen is not invented.** No evidence, no effects, no
  uncommitted count, no call sites: there was no run to watch and no working
  tree to count. Their defaults all render as "not known", which is the truth.

`attempt: 0` is the sentinel for "no attempt behind this review" — `attempts.id`
is a SQLite `INTEGER PRIMARY KEY`, so it starts at 1 and zero can never collide.
That keeps one shape on the wire, so a verdict can be fingerprinted, recorded
and printed against a review with no run behind it.

### The head ref is now on the row

`tasks.pr_head_ref`, written by the same poll that already wrote `pr_base_ref`
(gh#282) and free from the same response. For everything the board dispatched,
the attempt's own `branch` answers this; for a pull request nothing dispatched,
this column is the only record of which branch the work is on, and the review
header names it.

### The door

`comet_proto::view::board::reviewable` is now `attempts > 0 || pr_url`. The
`gh!191` row had no attempt, so no surface offered a way in — the fix is
worthless behind a door that does not open. A row with neither an attempt nor a
pull request is still not reviewable: nothing ran and nothing was pushed, and
the door would open on an empty room.

### The verdict goes to GitHub, and to nobody's chat

The question §gh#344 asked to be decided explicitly. A verdict on an
undispatched pull request is recorded, carries the remainder, and is posted —
and it is typed into no session at all.

Not because delivery is hard. Where comet *can* recognise the chat that opened a
pull request from its checkout and branch, the poll's session adoption already
records an attempt for it, and from that point the verdict is delivered exactly
as a dispatch's is. What the board will not do is guess. With no attempt there
is nothing for `still_the_authors_checkout` to check a chat against, and a
review typed into a session on the strength of a plausible match is a review
delivered to whoever happens to be sitting in that session — the precise hazard
that check exists to prevent.

The receipt says so in words that do not describe a chat that was lost:

```
nothing dispatched gh!191, so the board knows no chat that wrote it —
the verdict is on the pull request
```

### What the surfaces say

The review header's third fact reads `no attempt · opened outside the board`
instead of `attempt 1 · still running`, which would have been two inventions in
four words. The claims section says nobody was ever told the contract. The
remainder block carries one line of provenance — *from GitHub's file list for
the pull request; nothing ran here to read a checkout of* — for the same reason
the recorded-diff line exists: a count is only as good as where it came from.

The phone reimplements this reading (`ReviewModels.swift`,
`BoardModels.swift`), so the cross-language fixture carries the case: one
undispatched review and three review-door rows in
`apps/ios/Comet/Spec/review-spec.json`. As ever, only the Rust half runs in CI —
`scripts/ios-review-spec.sh` is the half a person has to run.

### Not in this issue

gh#340 — why the agent did not dispatch — is untouched, and is the half of this
that can be designed out. §gh#339 taught the *brief* to ask for claims, which is
a fix for runs the board starts; it cannot reach a pull request nobody
dispatched, and that is exactly the gap this issue fills from the other end.

Nothing here tries to attribute authorship more aggressively than the existing
session adoption does; the recovery path this issue leaves open — matching a
chat by cwd *and* branch — is that adoption's, and it is deliberately the only
way an undispatched pull request ever gets a chat.
