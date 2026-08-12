# The merge endpoint that cannot merge a stack — **done** (gh#290)

Stacks 9/9, closing what 2/9 (gh#283) named on its way out: the board could say
`ready to land` about a layer of a stack and had no endpoint that could carry it
out. `Github::merge_pr` called `PUT /repos/{repo}/pulls/{n}/merge`, which GitHub
documents as unable to merge a stacked pull request at all.

Landed in `crates/board/src/sources/github.rs` (the endpoint, the status type and
the poll), `SyncEngine::merge_pull_request`, and `merge_confirmation` in
`comet_proto::view::board`.

### The endpoint is a different contract, not a different path

GitHub's asynchronous merge API is not a rename. Three things change:

- **Submitting is not merging.** `PUT .../merge-async` checks only that the pull
  request is open and not a draft, and answers `pending` with a uuid. The branch
  protection rules are evaluated later, when the merge actually executes. So the
  board polls `GET .../merge-async/{uuid}` until GitHub stops saying `pending`.
- **The answer has four shapes**, and only one of them is a merge: `pending`,
  `merged`, `enqueued` (the stack went into the base branch's merge queue, which
  can still reject it) and `failed`. `MergeStatus` carries the first three;
  `failed` is an `Err`, because a caller that has to remember to check for it is
  a caller that will eventually forget.
- **A merge is a group.** Merging layer 3 of 5 merges — or queues — layers 1, 2
  and 3, atomically: all of them land, or none of them do.

### What the board does with a merge it did not watch land

Only `merged` marks the row. A queued merge and a merge still running when the
wait is over both leave the row in review and say so, because neither has
landed. Nothing is lost by that: `link_pull_requests` reads `merged` off every
pull request the poll fetches and calls the same `finish_on_merge` the merge
command does — the path a merge made on the web already takes. A `merge_pending`
marker column was considered and dropped for exactly that reason: it would hold
a fact GitHub is already answering, and the poll would have to reconcile it.

The wait itself is bounded — a second between polls, twenty of them — and the
bound is set by how long a keypress may block rather than by how long GitHub may
take. A stack merge is documented as taking up to a few minutes, and the honest
answer at the end of the budget is "still merging", not a guess.

### `merge_method: "merge"`, now on purpose

The hardcoded merge method was accidental protection. It is now deliberate, with
a comment and a test standing on it: **squashing a mid-stack layer rewrites the
commits every layer above it is built on, and the stack does not survive it.**
Whoever makes the merge method configurable has to come through
`the_merge_method_is_merge_because_a_squash_destroys_a_stack`.

The issue's open question — *does the async submit accept a merge-method
override, and do we ever want anything but `merge`?* — checked against the API
docs while implementing:

- **It does.** `merge_method` takes `merge`, `squash` or `rebase` on the
  asynchronous endpoint too, so the protection really is ours to keep or lose.
- **Squashing a *whole* stack in one operation may be legitimate** — one
  landing, one commit, nothing above it left to rebase. **Squashing a layer with
  other layers on top of it is not**, under any option. If the method ever
  becomes configurable, that is the distinction it has to encode: it is a fact
  about the pull request's position, not a preference.
- `merge_action` is left off, so GitHub's own default picks between a direct
  merge and the merge queue — it knows which the repository is set up for. Its
  one known edge is that a merge method is not supported on a queued merge, so a
  repository whose default routes into the queue may refuse the body. That
  refusal now arrives with GitHub's words in it (see below) rather than as a
  bare status.

### A refusal has to arrive with its reason

`Rest` grew `put_reply`, which hands back the status alongside the body. The
asynchronous merge is the one place where the status code is part of the
*answer* rather than a verdict on the call — 202 accepted, 200 already done, 409
somebody else's submission in flight, 400 not in a state to merge — and the last
two carry GitHub's reason in a body that the ordinary path throws away for an
`HTTP 400` with no words in it. `github refused the merge (HTTP 409): a merge
request is already enqueued` is an operator's next step; `github HTTP 409` is a
support ticket.

Credential and rate-limit failures still mean the same thing at every endpoint
and never come back as data.

### The confirm step

`merge_confirmation(row)` is the sentence the confirm has to carry, and it exists
because 1/9 and 2/9 put the stack on the row: the board knows which pull requests
a merge takes with it and names them.

```
merge gh!13 into main · this lands PR #11, PR #12 with it — GitHub merges the
group or none of it · clean against board/gh-12-parser · waiting on PR #11
```

The sentence opened `merge PR #13` when it shipped. gh#357 made it name the task
first — `merge gh#353 (PR #13) into main`, and `merge gh!13 into main` where the
row's name already *is* the pull request. The layers it takes with it stay pull
request numbers: they are locations, and the task at each one may not be a row
on this board at all.

Only the layers *below* appear — the ones that come along. The layers above are
untouched, and an already-merged layer is history in the chain rather than cargo.
The board's own reservation comes last and only when it has one: `ready` adds
nothing after the sentence has already said what merging does, and a reader who
opened the confirm on a layer the board can see is stuck is the reader most in
need of the words, since GitHub evaluates its rules at execution time and nothing
upstream will stop them.

### Not in this issue

Nothing calls `merge_pull_request` yet — it was test-only before this and it is
test-only after. The review screen's verdict bar (gh#234) is what wires it to a
keypress, and it now has an endpoint that works on the pull requests the stacking
work produces and a confirmation sentence to put above the button.
