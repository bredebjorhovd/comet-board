# The stacks sequence meets a real stack — **done** (gh#337)

Nine issues of stack handling landed against fixtures. This is the first time
any of it met GitHub. The rig is `scripts/stacks-rig.sh` (the reproducible half)
plus `bredebjorhovd/board-scratch`, which now holds the pull requests it was run
against — real ones, still there to look at.

Run on 2026-08-14 against `main` at 84f2623 (v0.7.0), a headless engine on
`COMET_IPC_PORT=27931` with its own `COMET_DATA_DIR`, never the box's.

**Four real stacks were built with `gh stack`, not by hand:**

| stack | layers | shape |
| --- | --- | --- |
| #7 | PR 4 → 5 → 6 | three undispatched layers, one row each |
| #12 | PR 9 → 10 → 11 | an agent-authored stack: `board/gh-1-…`, `-2`, `-3` |
| #15 | PR 13 → 14 | two dispatched rows, `--onto` then `gh stack link` |
| #20 | PR 18 → 19 | the same, for the upper-layer cases |

Plus PR 8, unstacked, as the control row.

### The matrix

20 of 23 cases were exercised against real payloads. **Sixteen hold outright**,
**one turned out to be the matrix being wrong rather than the code**, and the
run **filed five defects** (gh#407–gh#411). Three cases could not be reached
from this Mac at all, for reasons that are themselves worth reading. Case by
case:

| # | case | verdict |
| --- | --- | --- |
| 1 | a stack arrives as one thing | **holds** — three rows, each `n of 3`, base `main`, the full `#4 ↑ #5 ↑ #6` map on every one |
| 2 | a mid-stack `clean` layer reads `waiting on PR #N` | **the matrix was wrong** — see below |
| 3 | the bottom layer reads ready | **holds** |
| 4 | an unstacked PR is unchanged | **holds** — no `stack` key at all, byte-identical row shape |
| 5 | changes on layer 1 → layers above derive `blocked`, notice reaches their chats | **holds** for dispatched layers; **gh#409** for undispatched ones |
| 6 | the notice is a notice, not the review body | **holds** — proven per chat, see below |
| 7 | force-push → replay → re-foot `base_sha` on evidence | **re-footing holds; the replay does not happen — gh#407** |
| 8 | a retarget costs no feedback refetch, and the log says why | **holds** |
| 9 | approving layer 1 clears the children by itself | **derivation holds**; the approve path is unreachable here, and dismissal does not clear — **gh#411** |
| 10 | changes on an upper layer propagate nothing downward | **holds** |
| 11 | updating an upper layer does not disturb the rows below | **holds** |
| 12 | merging through the async API returns `pending`/`enqueued` | GitHub's half **holds**; the board's half is **unreachable — gh#408** |
| 13 | a queued merge lands the row on the next poll | **holds** |
| 14 | the once-only rewrite notice, naming branch, risk and two commands | **holds** |
| 15 | GC holds the parent branch while a child stands on it | **holds** |
| 16 | `pr_merged` only when every layer landed | **holds** |
| 17 | a layer closed without merging is neither open nor merged | **holds** |
| 18 | the count stays GitHub's; the map has a hole rather than shrinking | **holds** — reached by a different road than the one the case names |
| 19 | two repos, one stack number | not exercised live; see below |
| 20 | an agent-authored stack links home to one attempt | **holds** |
| 21 | `--onto` an unpushed parent refuses, and says why | **holds** — and **gh#410** |
| 22 | retrying a stacked task warns rather than refuses | **holds** |
| 23 | a reaped chat → `landing` answers `changes-below` on the row | **partly** — the precedence holds; the reaped branch was not reached |

### Case 2: `ready to land with 1 below` is the truth, and gh#388 resolves

Every layer of a real stack reports `mergeable_state: clean`, positions 2 and 3
included — which is what gh#388 filed, and this rig reproduced it on the first
poll. The matrix said a mid-stack `clean` layer must read `waiting on PR #N` and
never "ready to land"; `landing` reads it `Ready { below: 1 }` and prints
`ready to land with 1 below`.

The board is right. Merging PR #5 — the **middle** layer of stack #7 — through
`merge-async` merged **#4 and #5 together**, atomically, and retargeted #6 onto
`main`:

```
PUT /repos/…/pulls/5/merge-async  → {"status":"pending","details":{"uuid":"c17ab7e0…"}}
GET …/merge-async/c17ab7e0…       → {"status":"merged","details":{"sha":"6ae1047…"}}
#4 merged_at 2026-08-14T21:40:21Z   #5 merged_at 2026-08-14T21:40:22Z
```

So `Ready { below }` means exactly what its docstring says — merging this lands
`below` of them with it — and "waiting on PR #4" would have been the false
answer. gh#388 can close: `clean` on a mid-stack layer is raw data the review
screen interprets, and `landing` is that interpretation, already correct.

### Case 6, proven per chat rather than argued

The doc snapshots were split per chat and searched for the notice text and for
the review body:

```
157ec820 (gh#3, the child)   notice=1  review-body=0
a259f088 (gh#2, the author)  notice=0  review-body=3
60d7723d (gh#1, unrelated)   notice=0  review-body=0
```

The layer above was handed the fact and never the feedback. The log says the
same thing from the other side: `gh#2: delivered 1 review comment(s) … into chat
a259f088` beside `gh#2: told 1 layer(s) above it that …#13 was asked to change`.

### Case 7: the re-footing works; the replay it is written around does not

`base_sha` behaved exactly as designed. After layer 1 force-pushed, the board
did **not** move the stamp on the strength of the poll. When the child's
checkout was actually rebased, the next thing that measured it re-stamped on
that evidence:

```
attempt 5: board/gh-3-third-line-agent was rebased under its recorded base
5e8541a09e1f — measuring from 78bc8a24c2bdc6745346eeb0c6f5351fce79edca instead
```

and `comet-board review` then reported **1 changed file** — the child's own line,
not the parent's. Layer 1's commits were not swallowed.

What is not true is the sentence the whole notice is built on. See gh#407.

### Case 18, reached from the other side

The case describes a stack reaching a repo the board does not poll. GitHub
stacks are per-repository, so that shape cannot be built — but the **same code
path** produces the same hole for an agent-authored stack, and that one is
ordinary. `gh#1` carries GitHub's `size: 3` with a `layers` map of exactly one
entry, because layers 2 and 3 are the same row's work rather than rows of their
own. The count stays GitHub's and the map has a hole. `Stacks::row_stack` never
reads `layers.len()`, which is the property the case is about.

It is also a live question for #234: the row that most needs the `#9 ↑ #10 ↑ #11`
map is the one that cannot draw it.

### Case 19, argued rather than run

Not exercised — it needs a second repository, and the account has none to spare.
What the real payload does show is that the risk is not hypothetical. GitHub
draws stack numbers from the **same sequence as issues and pull requests**:
board-scratch produced stacks 7, 12, 15 and 20, interleaved with its PR numbers.
Any second repository reaches those numbers within a few weeks of ordinary use,
so `(repo, number)` is load-bearing rather than defensive. The unit test in
`stacks.rs` is what stands behind it.

### What could not be reached from this Mac

- **A second reviewing identity.** GitHub refuses `REQUEST_CHANGES` and
  `APPROVE` on your own pull request, and every pull request here is
  bredebjorhovd's — the Mac has no App credential (comet-board's gh#58 path
  exists; herdr-board's does not). The way through was a workflow casting the
  review as `github-actions[bot]`, which needed board-scratch flipped public
  because the account's Actions are billing-blocked. That covers
  `REQUEST_CHANGES`; **Actions is refused `APPROVE` outright**, so case 9's
  literal path has no route on this box at all.
- **Case 9's substance still holds.** `blocked` cleared itself with no
  bookkeeping when the parent's pull request stopped being open —
  `gh#3: blocked → review` on the poll after #13 merged, from `changes_below`
  walking past a closed layer.
- **Case 23's reaped chat.** Reaping a chat needs a surface this rig has no
  headless access to. The precedence it is really about was observed: gh#3 read
  `pr_mergeable: clean`, `changes_below: 13`, `landing: changes-below` — the row
  carried the fact ahead of GitHub's own verdict.

### Filed out of this

- **gh#407** — the notice tells a stacked child to wait for a replay GitHub only
  performs on a merge. A force-push of a lower layer leaves the upper branch
  untouched and its pull request `dirty`. The headline finding: the notice fires
  at exactly the moment its own advice is wrong.
- **gh#408** — nothing can merge from the board: `merge_pull_request` and
  `merge_confirmation` have no caller outside their own tests.
- **gh#409** — an undispatched pull request never fetches its reviews, so a
  stack of them propagates nothing — the rows gh#344 made reviewable.
- **gh#410** — a dispatch refused before it cuts a branch still burns an attempt.
- **gh#411** — a dismissed `changes requested` never clears, so the layers above
  it stay blocked until the parent merges.

Case 2's answer closes **gh#388**.
