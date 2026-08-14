# Every layer said clean, and none of them read that way — **done** (gh#388)

The confirmation §gh#337 asked for, against the first real GitHub stack the
board produced: **stack 49 on `Florin-AS/orion-productmapping`**, three layers,
PRs 47 → 48 → 50, opened the morning of 2026-08-13 and merged the same morning.
No fixture was invented for it — the payload is recorded in
`crates/board/fixtures/gh-388-stack-49.json` and enters the tests as the JSON
`GET /pulls` returned.

### The answer

**No defect. The review screen never surfaces the flat `clean`.** Every layer of
stack 49 reported `mergeable_state: clean`, positions 2 and 3 included, and what
the board renders is the verdict derived from it (§gh#283), which reads:

```
1 of 3 · ready to land · in PR #47
2 of 3 · ready to land with 1 below · in PR #48
3 of 3 · ready to land with 2 below · in PR #50
```

The count is the part that makes `ready` honest. "Ready to land" alone would
claim that this pull request lands by itself; PR 50 lands three. The word
`clean` appears on no row, and the confirm on the top layer names the cargo
before the one key that cannot be undone:

```
merge gh#46 (PR #50) into main · this lands PR #47, PR #48 with it
  — GitHub merges the group or none of it
```

### Why `ready` and not `waiting on PR #47`

§gh#337's case 2 says a mid-stack layer whose own `mergeable_state` is `clean`
reads `waiting on PR #N`, **never** "ready to land". That is right about the
payload it was written for and wrong as an unconditional rule, and §gh#290 is
what changed it: GitHub's asynchronous merge takes a layer and lands *every open
layer beneath it*, atomically. So a clean layer over a clean layer really can
land, and the AND in `landing` says so; `waiting on PR #N` is what a layer over
a layer GitHub **objects to** reads, and
`a_clean_child_over_a_dirty_parent_is_not_ready_to_land` in `stacks.rs` is where
that case is held. Stack 49 never entered it — nothing under any layer was ever
dirty, behind or unread — so this payload could not have exercised case 2 as
written, whatever the code did.

The stack's own ending is the evidence for the semantics the wording rests on.
The three layers merged at **09:15:08, 09:15:09 and 09:15:11**, one merger, each
into its own base, the whole chain reaching `main` — one group merge from the
outside, three seconds wide.

### What the reporter was looking at

The board's rows carry both fields. `pr_mergeable` is GitHub's raw answer and
`landing` is the board's verdict (`ready`, `waiting-on-stack`, `not-clean`,
`changes-below`, or absent for "nobody has asked"), and `list --json` has
carried both since §gh#283. The summary in the ticket printed `mergeable=clean`
and not `landing=ready`, which is the whole of the doubt: **on a stacked row,
`mergeable` is the field that does not answer the question**. The printed
`list` does not have the same trap — it puts the sentence on the row itself,
`(3 of 3, ready to land with 2 below)`, and only for stacked rows, because a
standalone pull request's `mergeable_state` means what it appears to mean.

### What landed

- `crates/board/fixtures/gh-388-stack-49.json` — the recorded payload, in two
  snapshots: `open` (the three layers as the ticket found them, all `clean`) and
  `merged` (verbatim, after they landed), plus the same repository's most recent
  unstacked pull request as the control. Titles and bodies are withheld: the
  source repository is private and this one is not. The fixture's `_provenance`
  says which three fields of the `open` snapshot were restored to their values
  inside the window, and on what evidence.
- Five tests in `sync.rs`, end to end over that payload — parsed by
  `Github::pulls`, linked by `link_pull_requests`, read back through
  `board_rows`, asserted as the words `row_metadata_line` prints. They cover
  §gh#337's read-path cases 1, 3 and 4 against real data, and case 2's
  all-clean sibling, which is this ticket.
- `the_top_of_an_all_clean_stack_says_how_many_it_would_land` in the TUI's
  render tests — the same shape drawn on a screen, list row and panel, since
  the question was about what a reader sees.

### The incidental one

Stack numbers and pull request numbers share a sequence, so they cannot collide:
this stack took 49 and the next pull request opened as 50. Nothing keys on that,
and nothing should — §gh#283's key is `(repo, stack number)`, and a stack number
is only ever compared with another stack number.
