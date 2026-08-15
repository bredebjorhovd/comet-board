# The merge key exists — **done** (gh#408)

Found by the gh#337 rig: gh#290 built the asynchronous merge end to end —
`Github::merge_pr`, the poll, the four statuses, `SyncEngine::merge_pull_request`,
`merge_confirmation` — and nothing called it. No CLI verb, no RPC, no key on any
surface. The board could say `ready to land with 2 below` and could not land it;
every merge in the rig run was `gh api -X PUT …/merge-async` by hand. The
endpoint was fine (the rig proved all four statuses against real stacks, and a
mid-stack merge really landed the group). What was missing was the surface.

Landed as one path with two doors: `MergeTask` on the RPC surface,
relay-forwarded like every other board verb; a `Merge…` key in the review
screen's verdict bar (gh#234's screen, exactly where gh#290's closing note said
it belonged); and `comet-board merge --task <id>`, which is the door the rig and
the orchestrating agents use.

### The confirmation is the caller's, the execution is the box's

`MergeTask` executes where the board's store and GitHub credential are, and it
adds no second question: the confirmation happened on the surface, before the
call. That split is deliberate. The confirm's job is done the moment the reader
agrees to a *sentence*, and the sentence — `merge_confirmation`'s wording, which
for a layer of a stack names every open layer that merges along — has to be
rendered where the reader is, off facts that surface already holds. An engine
that asked again could only ask with the same words, one round trip too late to
change anything.

The reply is one sentence a surface shows verbatim: `o/r#87 merged`, `o/r#87 is
in the merge queue`, `o/r#87 is still merging`. Only the first moved the row
(gh#290's rule: the board never records a merge GitHub can still reject); the
other two land on the board when the poll sees them, the same path a merge made
on the web takes. A refusal arrives as the call's error with GitHub's words in
it (gh#338).

### One sentence, whichever screen the key is on

`merge_confirmation` took a `TaskRow`, and the review screen does not hold one —
it holds an `AttemptReview`, a different shape about the same pull request. The
gh#389 move applies verbatim: the function now reads a `MergeSubject` — the
row's name and address on top of the five `Stacked` facts — and both shapes say
how they spell it (`From<&TaskRow>`, `AttemptReview::merge_subject`). The review
spells its pull request number off its own URL (`pr_url_number`, `pr_repo`'s
other half). Pinned by a test that walks a three-layer stack asserting the
review's sentence and the row's are byte-identical, so which screen a reader
pressed the key on cannot change what they were told they agreed to.

### The key on the review screen

In the verdict bar, far left, quiet: landing the row is the bar's other act, and
the bar's one solid control stays the verdict's Submit. The ellipsis in `Merge…`
is the promise that a confirmation follows — the click arms the dialog, never
the merge. The dialog carries the sentence and two buttons, and the outcome line
stays on screen the way the verdict's receipt does, settled-green only when the
answer was `merged`.

The key is drawn whenever the review has a pull request and the row is not done
— deliberately *not* gated on `landing` being `Ready`. The board's reservation
rides inside the confirmation instead, because GitHub evaluates its rules at
execution time and a board that hid the key on a stale poll would be refusing a
merge GitHub takes. Done is different: a merged pull request has nothing left to
press.

### The verb

`comet-board merge --task <id>` asks first, on the row's own words, prompting on
stderr so a `--json` caller's stdout stays clean. Without a terminal it refuses
rather than assumes — a hang on a pipe nobody is writing to is not a question —
and `--yes` is the explicit way past it, which is what an orchestrating agent
passes. The row is read before anything else: the confirmation is worded off it,
and a task the board does not hold has nothing to confirm about.

### On the loop, like every other write

`Msg::Merge` runs on the board loop's thread — one writer of `board.db` is the
rule — and holds it for up to the poll budget (twenty seconds), the same order
as a dispatch's clone. A merge that lands publishes rows immediately: the
operator just pressed the key and the row has to move now, not on the next
cycle.

### Not in this issue

The TUI board pane has no merge key yet — its path is `comet-board merge`, which
reaches the same board over the same forwarded RPC. And the merge executes under
the board's GitHub credential, not the presser's: gh#369 made *verdicts* carry
the reviewer's identity, and whether a merge should follow is a separate
question nobody has asked yet.
