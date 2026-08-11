# Changes requested on the parent reaches the children — **done** (gh#289)

Stacks 8/9. Review delivery was strictly one pull request → one authoring attempt
→ one chat. In a stack that contract is incomplete: `changes requested` on layer 2
is not only about layer 2, because layers 3..N are built on code that is now
wrong. Layer 2's agent got the review and started fixing; when it force-pushed,
GitHub replayed every layer above it on the new base — so their diffs moved under
their authors *and* under their reviewers, with no explanation in either place.

Landed as [`crate::stacks::Dependents`], [`crate::review::address`] and
[`crate::review::compose_notice`] in `crates/board/src/{stacks,review}.rs`, a
`changes-below` verdict in [`comet_proto::view::board::landing`], and one arm in
[`crate::model::derive_state`].

### The mechanism, not the notice

The constraint the issue was dispatched under: **build the fan-out as a way to
address the other layers of a stack, not as a special-case notice to children.**
This is where one-PR-one-chat first bends, and how it bends decides how cheap the
stack review surface (#281) is later.

So the two halves are separate and both are general:

- `Dependents` is the *edge*. Given one board's worth of tasks it answers "who
  else is a fact about this row a fact about" — `above` for the layers built on
  it, `changes_below` for the nearest layer underneath that is holding it up.
- `review::address` is the *address*. Given a row and its pull request it answers
  "where can this layer be reached": the attempt that wrote it and a chat verified
  alive and still standing in that attempt's checkout. The authoring delivery and
  the fan-out both go through it, so the author check cannot drift from the
  fan-out of it. It replaces the inlined check `deliver_review_for` used to carry.

The notice itself is then three lines of composition and a per-edge watermark.

### Direction: downward only

Propagation follows the dependency edge, and dependency points one way. Changes
requested on layer 2 invalidates layers 3..N. Changes requested on layer 3
invalidates **nothing** about layer 2 — it is still correct, still mergeable,
still reviewable — and telling its agent would be noise it cannot act on.

The seductive upward case (reviewing layer 3 reveals that the real fix belongs in
layer 2) is a human deciding to request changes on layer 2 as well. Automating it
means guessing which layer a complaint belongs to, and a wrong guess sends an
agent to edit the wrong branch.

### Two edges, unioned

A stack reaches the board two ways, and the dependency is real either way:

- **GitHub's own stack object** (gh#282), grouped into ordered chains by
  `Stacks` (gh#283). Each layer is stacked on the one below it.
- **`attempts.stacked_on`** (gh#285) — a dispatch the board cut from a sibling's
  branch. GitHub is never told that is a stack, so its `stack` object is absent
  and the grouping finds nothing; the child's diff is no less dependent for it.

Where both speak, GitHub's wins: it is the topology the rebase will follow. The
edge is an attempt id and not a branch string for gh#285's reason — a parent that
merges has its branch deleted, and this is one of the two places that needs the
edge precisely then.

`above` is transitive and nearest-first. Transitive because rewriting layer 2
moves layer 3, which moves layer 4: a notice that stopped at the direct child
would leave every layer above it wondering why its diff moved, which is this bug
one layer up. Both walks are bounded by the number of rows, so a malformed edge —
two rows each recorded as the other's parent — costs a short answer and never a
hung sync cycle.

### Question 1 — hold or inform: split by state

- **A child in `review` comes out of review.** A human reading a diff that is
  about to be rebased is wasted attention, and the worse outcome is not wasted
  reading: it is an *approval that outlives the diff it was given to*.
- **A child still `working` is informed, not stopped.** Its run may produce work
  that survives the replay, and killing an in-flight agent to save it a rebase
  spends a context to save a `git rebase`.

That split falls out of `derive_state` for free, because rule 3 already answers
`working` for a live working attempt and only reaches the review decision for a
settled one. The new arm is one function, `reviewable`:

    open pull request + a layer below asked to change  →  blocked
    open pull request                                  →  review

**`blocked` and not a seventh board state.** A seventh state is six glyphs, two
section orders, two viewports and a wire contract, for a condition that already
has a word: `blocked` is the board's name for work that has stopped short of an
answer and needs somebody. The row goes back to `review` by itself — the fact is
*derived*, so it stops being true the moment the layer below is approved, merged,
or closed. Nothing has to remember to clear it.

The design comment named `settled::decide` as where the edge would have to reach.
It does not: `decide` answers whether an *attempt* is finished, and the attempt
genuinely is — the agent wrote the layer and opened its pull request. What changed
is what the *board may claim* about that finished work, which is `derive_state`'s
business, the same distinction gh#283 drew when it kept the stack out of the
settle.

### Question 2 — approvals do not fan out

An N-layer stack would generate O(N²) notices over its life, every one of them
"somebody below you is fine" and none of them actionable. What a child needs to
hear is that its parent *merged* and its base has moved, and gh#288 and gh#286
own that.

### Question 3 — an undeliverable notice is still delivered

The audience is not only the agent; it is the human about to review the child. If
the chat has been reaped the row is the only surface left, and the review screen
is where that person is looking. Dropping it silently recreates the original bug
one layer down.

So there are two deliveries, and the second one always happens:

- the notice into the child's chat, when there is one to address;
- the fact on the child's row, which `landing` answers with **before it answers
  anything else** — ahead of GitHub's `mergeable_state` and ahead of "nobody has
  asked". `clean` is the one answer that gets somebody to press merge, and every
  fact `mergeable_state` reports is measured against a branch that is about to be
  a different branch. `landing_note` words it as *"PR #12 below was asked to change
  · this rebases under it"* — not "waiting on", which is what a dirty parent
  earns, because the point is that the diff moves rather than that a merge is
  queued behind something.

`Landing::ChangesBelow` therefore also reaches `merge_confirmation` for free,
which is the other place a reader is about to do something irreversible.

One surface needed a line of its own. The row it lands on is `blocked` with no
agent in it and no clock to run, and the `blocked` metadata arm had neither a pull
request number nor a landing note in it — so without help the row would sit in the
loudest section of the board saying nothing at all about why it is there. That arm
now carries the landing note whenever `changes_below` is set, which covers the
`working` layer too: informed rather than stopped still means informed.

### One fact, two homes, one writer

The stored fact is `Delivered::changes_requested` — the id of the review that last
asked this pull request to change, `None` when nothing is outstanding. It lives in
the delivery record because that is what the record already is ("what has been
said to this agent about this work"), beside the watermarks it is computed from.

`review::store` mirrors it onto a `tasks.pr_changes_requested` column, and is the
only writer of that column. The record is the source; the column is what the state
derivation and every board read see without a `meta` lookup per row — the shape
`pr_mergeable` already has, for the same reason: the rows *above* this one read it.

It is computed off the **raw fetch**, not the delivered subset, because the two
filters delivery applies are wrong for a verdict: an approval with nothing written
in it interrupts nobody (`is_actionable` drops it) and is still exactly what ends
the objection, and a `changes requested` older than the attempt's end is below the
first-sight floor and still outstanding. Only `changes_requested` and `approved`
count — a `commented` review says nothing about whether the objection stands, which
is GitHub's own review-decision semantics.

### The board's own review window feeds the same path

§gh#239's `submit_verdict` posts a review and **watermarks its own id
immediately**, so the inbound pass can never read it back. Left alone, that would
mean the one way Brede actually requests changes is the one way that never fans
out. So `submit_verdict` sets the standing verdict directly (`ChangesRequested` →
the id, `Approve` → cleared, `Comment` → untouched) and the fan-out runs off the
standing fact rather than off a review just read. One path, two sources.

It also re-derives on the spot. A reviewer who just pressed the button should see
the layers above leave the review section now rather than on the next poll — and a
derivation that fails there is a log line, not a failed submission, because the
review is already on GitHub. `deliver_reviews` does the same once at the end of its
pass, and only when a standing verdict actually moved: that pass runs *after* the
cycle has already derived every row, so without it the rows above would look
reviewable for one more poll interval.

### The watermark is per edge

`Delivered::fanned_out` is a map from dependent task id to the id of the last
review of this pull request fanned out to it — not one watermark for the fan.
"Has this layer been told?" is a fact about that layer: a dependent whose chat was
unreachable this cycle is retried, and a layer dispatched onto this branch *after*
the review landed still hears about it. One watermark would call the whole fan
done on the strength of the first success.

Recorded as told when the chat cannot be addressed (the row has it, and retrying
forever would log the same line every thirty seconds) and **not** recorded when
the ledger refuses the prompt, which is a failure with a retry in it. The
distinction is the same one `deliver_review_for` already makes for the author.

### Cost

Nothing. The fan-out asks GitHub nothing: it runs off the standing verdict on the
row and the pull requests the sync cycle already polled. Its gate is
`task.pr_changes_requested.is_some()`, a column already in hand, so a board with
no outstanding requests pays one field test per row. `Dependents` is built once per
pass, exactly as `Stacks` is.

A verdict this cycle's own fetch discovers fans out in the same cycle rather than
the next one: the gate at the top of `deliver_review_for` reads the row as the
cycle loaded it, so there is a second call after the store for the case where the
fetch just found one.

### What is not in this issue

- **A `changes requested` on a row nobody dispatched does not fan out.** The board
  learns a pull request was asked to change from the pass that delivers its review,
  and that pass returns early for a row with no authoring attempt — there is
  nobody to deliver to. Closing it means fetching three comment endpoints for
  every imported pull-request row on the board, which is the cost gh#288 just
  removed. A row the board dispatched is the parent case that matters, and
  gh#287's `link_for` already collapses an agent-authored stack into one such row.
- **The same hole for a parent whose chat has been reaped**, and for the same
  reason: the fetch is gated on there being an author to tell. The row-level fact
  and the notice both wait until something asks GitHub again.
- **The stack review surface** (#281). This issue deliberately leaves it open by
  building the addressing rather than the notice.
