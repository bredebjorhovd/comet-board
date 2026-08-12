# A task has one name, and the pull request is a location (gh#357)

A task has two numbers and they never match. `gh#353` is the ticket; the pull
request that answers it lands on whatever number GitHub hands out next. One
night's run, as it happened:

| task | pull request |
| --- | --- |
| gh#338 | #345 |
| gh#341 | #347 |
| gh#342 | #346 |
| gh#343 | #350 |

Neither ordering predicts the other — tickets are numbered when filed, pull
requests when opened, and agents finish out of order — so a reader holding one
number cannot derive the other. That is fine as long as everything comet says
about a task agrees on which of the two is its *name*.

### The rule

**The task's identifier is its name everywhere comet speaks. A pull request
number is a location, and locations go after names.**

Three consequences, and they are the whole of this issue:

1. A row leads with its identifier, and the pull request is a detail on it.
2. A sentence about a task names the task, then locates it.
3. Where the board names a *sibling* layer of a stack, it names a location on
   purpose: `waiting on PR #11` is right, because the task at PR #11 may not be
   a row on this board at all, and its identifier would name nothing the reader
   can look up.

### What was wrong

`row_metadata`'s review block led with the pull request, which gh#283 had put
there along with the stack half:

```
PR #12 · 2 of 3 · clean against board/gh-11-lexer · waiting on PR #11
```

Three pull request numbers and no task identifier, and the first token — the one
a reader grabs to quote — is the number that is not the task's name. gh#283 put
the stack facts there for good reasons; the position and the layer in the way are
genuinely about pull requests. The damage was the ordering.

The block now says what the row is, then what it needs, then where it lives:

```
2 of 3 · clean against board/gh-11-lexer · waiting on PR #11 · in PR #12
```

`in` is doing work. The row's own pull request had to move behind the landing
note to stop leading, and a bare `PR #12` sitting after `waiting on PR #11`
reads as the second item of a list. The preposition is what makes it an address
instead. The same reason gives `no PR · on board/gh-11-lexer` for a review that
finished on commits with no pull request raised — which used to lead with the
branch, another location.

`merge_confirmation` had the same shape and the higher stakes, since it is the
last screen before the one action the board cannot undo:

```
merge gh#353 (PR #13) into main · this lands PR #11, PR #12 with it — …
```

It was `merge PR #13 into main`. A reader who opened that from a row called
`gh#353` had two numbers to reconcile before they could tell it was the same
work.

### The `gh!` rows are not an exception

A pull request the board never dispatched has no ticket (gh#344), and its row is
called `gh!191`. That name is the pull request's number, and the rule still
holds: `gh!191` *is* that task's identifier, because the pull request is the only
thing that exists. What changes is that naming the row and locating it are one
act, so the surfaces say one of them —

- `TaskRow::is_pull_request` answers it: the id is the `!` form and its number is
  the row's pull request.
- The review row ends at `waiting on you` rather than `waiting on you · in
  PR #191`.
- The confirm says `merge gh!191 into main`, not `merge gh!191 (PR #191)`.

`display_identifier` was quietly breaking the same rule from the other end. It
humanizes a task id into the row's leading token (`tally #507`, gh#125) and it
split on `#` and `!` and re-emitted with `#` — so a `gh!508` row rendered `tally
#508` in the terminal and on the phone, which is a second name for it, and one
already spoken for by issue #508. The separator is now the id's own: `tally
!508`. (The desktop list never had this — its id column shows the raw
identifier, because it has a repo column of its own.)

### What was already right, and stayed

- Every notice in `notify.rs` opens with `task.identifier` and carries the pull
  request as a URL. The settle notice was the model for this issue, not a target
  of it.
- `reviewed_header`, the gh#289 stacked-child notice (`{identifier} · PR #{n} on
  {branch}`), and both review screens' headers — desktop `review.rs` and iOS
  `reviewContextLine` (`gh#117 · PR #9 · comet-board`) — already name first and
  locate second.
- `comet-board list` prints the task id and the pull request's URL.
- The desktop board's own PR column (`time_cell`) stays `PR #12`: it is a column
  under a heading, in a grid whose first column is the identifier — the same
  shape as the review screen's `gh#138 · PR #212`, which is the ordering this
  issue endorses.

### Not in this issue

The other half of what was asked for here — that the name be *informative*, not
merely agreed on — is gh#364, which carries a slug of the title beside the
identifier (`gh#341 review-page-loads`) and spends the branch's repo half on it.
It is decoration on the key and drops before the identifier does, so the rule
above is what it rests on rather than something it competes with.

The iOS row sub-line is a hand-port of `state_metadata_fields` and has been
drifting since gh#283 — it still carries no stack or landing facts. This change
keeps the port faithful for the facts it does carry (`waiting on you · in PR
#12`, `no PR · on <branch>`, the `gh!` case) and does not close the drift. There
is also no Swift spec runner covering `boardRowDetail`, so the Swift half of this
rests on the Rust tests and on review.
