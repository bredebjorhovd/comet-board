# A stack arrives as strangers — **done** (gh#283)

Stacks 2/9, on top of gh#282. Landed as `crates/board/src/stacks.rs` (the
grouping), `landing` and its vocabulary in `comet_proto::view::board`, the stack
map in both viewports, and `stack`/`landing`/`pr_mergeable`/`pr_base_ref` on the
published row.

1/9 taught the sync loop to read the `stack` object off the pulls it already
fetches and store it flat: number, size, position, target branch. That makes
every layer *say* it is in a stack. It does not make the layers know about each
other, and it does not stop the one fact a stacked pull request reports about
itself from being read as something it is not.

### Two bugs, one shape

**Siblings were unrelated rows.** A five-layer stack is five `gh!…` rows, each
deriving to `review` on its own, in whatever sections of the board their states
put them. Nothing said they were one change, and nothing on the row you had open
named the four whose merge state decides whether this one can land.

**`mergeable: clean` mid-stack is a lie the reader believes.** GitHub answers
`mergeable_state` per pull request, against *that pull request's own base*. For
a standalone PR the base is trunk and the answer is the whole story. For a layer
the base is the layer below it, so `clean` means "clean against the branch
underneath me" — and the person reading a board to decide what to merge reads it
as "ready to land".

Both are the same shape: a fact about one row whose meaning lives in the others.

### The grouping

`Stacks::of(&tasks)` makes one pass over the board and hands every member the
whole map — each layer's task id, identifier, pull request number, position,
whether it is still open, and its own `mergeable_state` — ordered bottom-first.
`board_rows` builds it once per frame, because a sibling is another row and no
amount of looking at one row finds it.

Three decisions inside that are worth keeping:

- **The key is `(repo, stack number)`.** GitHub numbers stacks per repository,
  the way it numbers pull requests. Grouping on the number alone would weld two
  repos' chains together and then answer "can this land" by ANDing somebody
  else's pull requests — the same class of bug as matching a PR to a task on an
  unqualified branch name (herdr-board AGE-20). A row that names no repository
  in its id gets it from the pull request's own URL; a row with neither is left
  out, because an unscoped stack number is not a weaker key, it is the wrong one.
- **The count stays GitHub's, never `layers.len()`.** A stack reaching into a
  repository the board does not poll is a map with holes. It still reports "3 of
  5"; the map just has three entries.
- **A layer with no position sorts last, not first.** An unplaced layer read as
  the bottom would tell everything above it that it has an unmergeable parent.

### What `mergeable` is allowed to mean now

`landing(row)` is the one implementation of the AND, and it lives in proto so
that the board fills the row's `landing` field with it, both viewports word it
with it, and a caller holding a row off the wire can call it. Its answers:

| | |
|---|---|
| `ready` | this PR is clean and so is every open layer under it — merging it lands the lot, which is what GitHub does |
| `waiting-on-stack` | clean against its own base and no further; the note names the branch, and the blocking layer when there is one |
| `not-clean` | GitHub objects to this PR itself; `pr_mergeable` says how, and the wording names the branch it was measured against |
| *absent* | nobody has asked. Mergeability costs a call per open PR and rides the full sweep, so this is the ordinary state of a fresh row |

Not knowing is never rounded up. A clean layer over an *unread* one is
`waiting-on-stack`, not `ready` — and so is a clean layer whose stack is deeper
than the map the board holds. The only path to `ready` is every layer below
being known open-and-clean, or already landed.

The wording always names the branch, because that is exactly the fact the flat
`mergeable_state` was missing: `clean against board/gh-11-lexer`, `conflicts with
board/gh-11-lexer`, `behind main`. A reader who sees only the first half of one
of those sentences has been told less than nothing.

### Where it shows

- **The row**, on both viewports: `2 of 3 · clean against board/gh-11-lexer ·
  waiting on PR #11 · in PR #12`. The landing note replaces `waiting on you`
  rather than joining it — both are the row's call to action and the one that
  names a branch says strictly more. (This block led with `PR #12` when it
  shipped; gh#357 moved the row's own pull request behind the facts, because
  leading with it made the location read as the task's name.)
- **The open row** carries `stack 2 of 3 · onto board/gh-11-lexer · lands on
  main` and the map itself: `#11 ↑ #12 ↑ #13`, this layer marked, a layer GitHub
  objects to in the failed colour, a landed one muted. On the desktop each chip
  is a door to that sibling's row; in the terminal `[` and `]` walk the chain,
  moving the cursor as well as the panel so closing it leaves you where you
  walked to. A layer the board does not hold is not a door and the keypress does
  nothing — a panel blanked to show a row that is not there is worse.
- **`comet-board list`** appends `(2 of 3, waiting on PR #11)` to a stacked row.
  Only stacked rows: the printed list never carried `mergeable_state`, and for a
  standalone PR it means what it appears to mean. `--json` carries `landing`,
  `stack`, `pr_mergeable` and `pr_base_ref` on every row.

### The two open questions

**Does a stack get one aggregate row, or the layers?** The layers. A stack is
not a task: nothing dispatched it, no issue backs it, no attempt settles it, and
a row with none of those is a row every other part of the board — derivation,
dispatch, GC, review delivery, the wall-clock cap — has to make an exception
for. The grouping rides the rows instead, and it rides *all* of them, so the
aggregate view the review screen (gh#234) wants is available from whichever
member it happens to be holding.

**Should a child whose parent merged derive differently?** No, and
`settled::decide` is untouched. A pull request is the agent's own statement that
its layer is finished, whether that layer sits mid-stack or was retargeted onto
trunk when its parent landed. What the stack changes is not whether the attempt
is over — it is what the board may claim about *merging* it, which is `landing`'s
business. The retargeted child is visible there rather than in the settle: its
merged parent stops being an obstacle the moment the poll says so, and the row
goes from `waiting-on-stack` to `ready` without anything having to re-derive.

### Not in this issue

Authoring stacks (3/9–6/9), review delivery across layers (7/9, 8/9) and the
asynchronous merge endpoint (9/9) are all still ahead. In particular the board
still merges through the legacy synchronous endpoint, which GitHub documents as
unable to merge a stacked pull request — so a `ready to land` on a layer is a
statement about GitHub's semantics, not a promise that the board's own merge key
can carry it out yet. That is gh#290's to close.
