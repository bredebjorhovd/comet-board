# A verdict is not GitHub's to veto — **done** (gh#365)

Seen on the box on 2026-08-12, board v0.5.0. Pressing **Approve** in the review
window on `Florin-AS/orion-productmapping#40` answered:

```
submitting the APPROVE verdict on Florin-AS/orion-productmapping#40:
github HTTP 422 for /repos/Florin-AS/orion-productmapping/pulls/40/reviews:
Unprocessable Entity — Review Can not approve your own pull request
```

That sentence is legible because §gh#338 taught the transport to keep GitHub's
words, and it confirms §gh#338's first hypothesis. `gh pr view 40 --json author`
says the author is `app/comet-board`, and the verdict is submitted through that
same App installation. GitHub is right to refuse: an App approving a pull
request its own App opened is not a review of anything.

Landed in `crates/board/src/verdict.rs` (`Projection`, `submit_verdict`'s
order, `SyncEngine::project_verdict`, `as_comment_body`, `projection_line`,
`receipt_line`), `refused_own_pull_request` in
`crates/board/src/sources/github.rs`, and the receipt on all three surfaces.

Two defects, and the second is the worse one.

### The board had no identity that may cast a verdict

`APPROVE` and `REQUEST_CHANGES` are both refused against your own pull request.
Only `COMMENT` is left. So two of the three buttons on the review surface were
dead on every board-dispatched pull request — which is all of them.
`VerdictKind::needs_comment` already knew Approve was the odd one out; nothing
knew the *identity* was.

### GitHub was load-bearing for a verdict that is mostly not GitHub's

```rust
let id = gh.post_review(&repo, number, kind.event(), &github_body(&review, comment))?;
```

The `?` was the whole bug. GitHub was the **first** writer and every other
consequence was downstream of it, so a refusal there lost:

- the watermark, the standing `changes_requested`, the `Submission` record —
  none of which are GitHub's to veto;
- the delivery into the checkout the agent is still standing in, the half
  §gh#239 exists for and the half GitHub has no part in;
- the comment the human had just typed.

A verdict is a *board* fact. It clears the objection, it unblocks the rows
stacked above it, it reaches the agent. Posting it to GitHub is a projection of
that fact, and a projection failing should not unmake the fact.

### Record, deliver, project

`submit_verdict` now writes in that order. The `Submission` and the standing
verdict land first, `rederive_all` runs off them, the payload goes into the
authoring chat, and GitHub is asked last. What GitHub says is written back onto
the submission as a `Projection` — `posted`, `posted_as_comment`, or `unposted`
with GitHub's own sentence beside it.

A refusal is then a visible unposted verdict rather than a lost one, and a retry
of the same submission finishes only the projection: same fingerprint, no second
record, no second prompt, no second review.

The inversion inherits the crash window the old order was avoiding — a process
that dies between the record and the post leaves a verdict GitHub has never
seen. That is the recoverable direction. The submission is on the ledger marked
`unposted`, and re-submitting the same words finishes it.

### The standing verdict needs a number before GitHub gives it one

`Delivered::changes_requested` is a watermark as well as a fact: §gh#289's
fan-out to the layers stacked above compares against it. A verdict recorded
before it is posted has no review id, so it stands under one the board makes up
— one above every id this pull request has ever been keyed by. Real review ids
come from the same counter and are in the billions, so a local id sorts above
everything already recorded and below the next real one.

When the projection does land, the local id moves onto GitHub's number, and so
does every `fanned_out` entry recorded under it. Both halves matter: without the
first, the inbound pass reading the board's own review back would disagree with
the board and re-fan; without the second, moving the id would tell every layer
above this one the same thing twice.

### The refusal the board can answer

`refused_own_pull_request` reads GitHub's own sentence — `Can not approve your
own pull request`, `Can not request changes on your own pull request` — and the
verdict is re-sent as a `COMMENT` whose first line says what it is:

```
**This is an approval.** GitHub does not let this App approve a pull request its
own App opened, so it is posted as a comment. comet-board has it recorded as
approved.
```

Then the ordinary body, unclaimed set and `POSTED_MARK` trailer underneath, so
the inbound path's backstop is untouched by the downgrade. Both refused verdicts
are answered, not just the one in the screenshot: fixing `APPROVE` alone would
leave one of the two dead buttons dead.

Read off GitHub rather than predicted from the pull request's author. Predicting
would cost a fetch of the author and one of the App's own slug on every
submission, and would still have to handle the refusal in the cases the
prediction gets wrong — a pull request opened by a human under a token the board
is not using, an installation that changed underneath it. GitHub is the
authority on who may review; asking it costs one round trip, and only on the
pull requests where the answer is no.

### What the reviewer sees

`verdict::receipt_line` moved out of `comet_ui` into `comet_board`, because
three surfaces print it and a verdict that reads as posted on the desktop and
unposted on the phone is the confusion this issue is about. It says the board's
half first and GitHub's second:

```
Recorded, and delivered into the chat once. It is on the pull request.

Recorded, and delivered into the chat once. It is on the pull request as a
comment that says it approves — GitHub does not let the board approve its own
pull request.

Recorded, and delivered into the chat once. It is not on the pull request —
GitHub refused it: github HTTP 500 for …
```

Nothing there asks the reader to know GitHub's rules about who may review what.
The cross-language fixture carries five of these cases now
(`apps/ios/Comet/Spec/review-spec.json`), so the phone's copy of the sentence
fails the runner rather than drifting.

One surface rule follows from it: the review window empties the comment box
after a submission, *unless* GitHub has no copy of it. Unposted, the words the
reviewer typed are the one copy a person can still do something with — retry
them, or paste them onto the pull request by hand. The retry is the same
submission and posts nothing twice.

### Not in this issue

Whether plain `Comment` works live is still unverified on the box; if it 422s
too, §gh#338's second hypothesis is alive and this is only half the story. The
board-side behaviour is the same either way — the verdict stands and the agent
is told — and the receipt now says which happened.

Branch protection is untouched. Nothing in `sync.rs` gates a merge on an
approving review, so none of this blocks merging; it blocked *recording*.

**The real fix is still ahead of this.** A verdict should carry the *human's*
GitHub identity, because a human is who gave it. The `[users]` map (§gh#162) is
already where a board member's GitHub login lives, and a user-to-server token
hung off the same entry would make Brede's approval say `bredebjorhovd` — and
GitHub would stop objecting, because the objection would stop being true. Its
own issue.
