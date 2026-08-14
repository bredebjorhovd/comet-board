# The review screen never said it was a stack — **done** (gh#389)

The first real GitHub stack this board produced went up on
`Florin-AS/orion-productmapping`: stack #49, three layers, PR 47 → 48 → 50. Every
board row carried the whole thing — number, position, size, target branch, and
every sibling mapped back to its task identifier — because gh#283 grouped stacks
a year of issues ago. Open one of those three rows on the review screen and none
of it was there. A reviewer saw an ordinary pull request. Nothing said it was
layer 2 of 3, nothing named the layers below it, and nothing linked to them; the
three rows read as unrelated work that happened to be dispatched at the same
time.

This was never a data problem. It is the one surface that had no way to ask the
question.

### Why the screen was the last to know

The board's list holds every row, so it can join a stack out of its own
neighbours. The review screen holds **one attempt** — `ReadAttemptReview`, one
task, one branch, one diff — and every fact on it was a fact about a single pull
request. There is no amount of looking at one attempt that finds a sibling.

So the join rides the review the same way it rides the row.
`stacks::place_in_stack` runs over the task list the review was resolved out of
and fills in two things: the stack map (gh#283's `Stacks`) and the nearest open
layer below that was asked to change (gh#289's `Dependents`). It uses the same
indexes `rows::board_rows` builds, from the same list, which is what makes it
impossible for the screen and the row to disagree about which stack a pull
request is in. `AttemptReview` grew four fields to carry it — `stack`,
`changes_below`, `pr_base_ref`, `pr_mergeable` — all absent on the wire for a
pull request that is not a layer, which is nearly all of them.

### One vocabulary, now readable from two shapes

`landing`, `stack_line`, `stack_note` and `stack_map` were functions on
`TaskRow`, because the board's list was the only surface that drew a stack. They
now read `board::Stacked` — id, stack, base ref, `mergeable_state`,
`changes_below` — and both shapes say how they spell those five facts
(`From<&TaskRow>`, `AttemptReview::stacked`). Nothing about the derivation moved.
That is the point: two implementations of "can this land" is the single failure
this vocabulary exists to prevent, and the review screen is the worst place in
the product for it, being the screen somebody is on when they decide to approve.

One function is new. `merge_order` says the thing no per-pull-request field can:

```
bottom-up: #47 lands before this one, #50 after
bottom-up: this is the bottom open layer — #48, #50 land after it
```

The rule is named once, at the front, and then said in this layer's own numbers.
Only open layers appear — a landed one is history in the chain rather than
something still to be sequenced, which is the same rule `landing` applies and
the same reason a retargeted child stops waiting the moment its parent merges.

### What it looks like

On the desktop card, a `Stack` band pinned above the scroll with the verdict —
not filed in the body, because a reviewer who has to scroll back past a long
issue body to learn this is a reviewer who will not:

```
Stack   stack 2 of 3 · onto board/gh-44-packages · lands on main
        #47  ↑ #48  ↑ #50
        bottom-up: #47 lands before this one, #50 after
```

The map is the board peek's chips, hue for hue: the layer you are on accented,
one GitHub objects to in the failed colour, a landed one muted. Each chip is a
door — to that layer's *pull request*, derived off this review's own URL, since a
stack is scoped to one repository and there is no board here to open a sibling
row on. A layer with no pull request number is not a door and nothing happens.

The header's facts line gained the landing note beside the branch, so `clean`
mid-stack never reaches a reader unqualified: `clean against
board/gh-44-packages · waiting on PR #47`, never "ready to land". `comet-board
review` prints the same three lines under the pull request URL, from the same
functions.

### What is deliberately not here

- **The phone.** `apps/ios` is a second implementation of the review reading in
  Swift, pinned to the Rust by `review-spec.json`. It decodes by explicit
  `CodingKeys`, so the four new fields are ignored rather than fatal — the phone
  is unchanged and correct about everything it does draw, and it draws no stack.
  Whoever adds it adds the Swift rules and the fixture cases together, or the two
  sides start disagreeing about merge order, which is the exact class of bug this
  issue is about.
- **Merging.** Nothing here merges anything; the board still merges through the
  synchronous endpoint gh#290 owns. `ready to land` remains a statement about
  GitHub's semantics.
- **Making the stack.** §gh#387 is why this only appears once somebody has linked
  the pull requests by hand: `--onto` never creates the stack object.
