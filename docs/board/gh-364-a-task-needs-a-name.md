# A number is an index, not a name — **done** (gh#364)

Split out of gh#357, which adopted the naming *rule* — the task's identifier is
its name, a pull request number is a location — and left the half that makes the
rule liveable unbuilt. From the operator, on the issue that settled the rule:

> Even `#357` isn't all that informative, especially during agentic loops. Any
> descriptive way to get more info at a glance that isn't taking up the whole
> board?

Four agents in flight is `gh#341 gh#342 gh#343 gh#356`: four rows that agree
about which number is the name and still look exactly alike. So a short slug of
the title now travels beside the identifier —

```
gh#341 review-page-loads
gh#356 settle-announced-twice
gh#359 unpriced-model-keeps
```

Landed as `comet_proto::view::slug::title_slug`, read by
`TaskRow::slug`, `AgentRow::slug`, `NeedRow::slug` and
`comet_board::dispatch::branch_slug`, with a line-for-line Swift port in
`apps/ios/Comet/Board/BoardModels.swift`.

**It is not a second identity.** It is derived, nothing is looked up by it, and
it drops before the identifier does on every surface that draws it. A generated
codename would have been more memorable and would have been a second thing to
learn and map back, which is the problem gh#357 exists to remove.

### The first three words are the wrong three

The obvious slug is the title's opening words, and it is *worse than the
number*, because English titles open with articles, auxiliaries and hedges.
Measured against this board's own issue titles:

| first four words | content words |
| --- | --- |
| `a-repo-can-end` | `repo-end-up` |
| `a-task-should-have` | `task-one-name` |
| `two-boards-can-be` | `two-boards-pointed` |
| `the-review-page-loads` | `review-page-loads` |

So the stopwords come out first, three content words go in, and the whole thing
is capped at 28 characters and cut between words. Roughly four in five titles
produce something genuinely useful; the rest come out harmless, which is the
trade this is allowed to make — the identifier beside it still carries the
meaning, so a poor slug costs a reader nothing but the width.

Three kinds of word most stopword lists strip are content here, deliberately:

- **Negations.** `a-retarget-is-not-news` is *about* the negation; a slug that
  drops it says the opposite of the title.
- **Particles.** They finish the verb they follow — `end up` is not `end`.
- **Quantities.** `a-github-only-board`, `one-name-for-a-task`: when a title
  mentions the count, the count is usually the point.

A word carrying non-ASCII letters is dropped whole rather than mangled.
`Ålesund` has no honest ASCII form, `lesund` is not the word, and a branch name
is not the place to invent a transliteration. A Norwegian title can therefore
produce no slug at all — which is a supported answer everywhere, because the
identifier alone is always enough.

### The branch was already spending that budget, on nothing

`branch_slug` was `slugify("{identifier}-{repo}")`, so a branch was
`board/gh-341-comet-board` and its worktree
`~/.comet-native/worktrees/comet-board/board-gh-341-comet-board`. **The repo
appears twice there and is implicit both times**: a branch lives in the repo it was cut
in, and the worktree already sits under a per-repo directory
(`Repos::create_worktree_on`). Branch namespaces are per-repo, so nothing
collides by dropping it. It now reads `board/gh-341-review-page-loads`, and
every `git branch`, worktree path and pull request head ref reads with it.

The repo was there for herdr-board AGE-20, where another repo's merged pull
request attached itself to an untouched task, and the assumption worth checking
before relying on it is whether anything parses the repo back *out* of a branch
name. Nothing does, and AGE-20's fix was never the branch:

- `sync::link_for` filters candidate pull requests by `own_repo` — the repo the
  **task id** names — before it looks at a branch at all.
- `AttemptBranches` keys its `in_repo` set the same way, off `gh_repo(&task.id)`.
- `stacks::layer_of` strips the whole attempt branch and reads the `-2`/`-3`
  suffix (gh#287); `link_pull_requests` matches `head_ref == attempt.branch`.

Both of the first two already carried a comment saying attempts recorded before
the repo qualification do not have it in their branch either — which is the same
statement as this change, arrived at from the other end.

### The cost of admission, and what pays it

A branch built from the identifier and the repo was built from two immutable
things. One built from the *title* is built on a field the tracker's owner can
edit at any moment, and nothing tells the board when they do. Left alone, an
issue renamed between two attempts would send the retry to a freshly cut branch
and leave the first attempt's commits — and its pull request — on a branch
nothing points at.

`resolve_branch` therefore reuses a branch this task already holds, deciding
"the same template" by the **stem**: the template with the descriptive half
emptied, `board/gh-341`. That is the only part allowed to move; a branch that
does not start with the stem was named by a different template or by nothing
here, and reusing it would be a guess. Attempts made before this issue land on
the rule for free — `board/gh-341-comet-board` starts with `board/gh-341` — so a
task in flight when the box updates keeps the branch it is working on.

### Where it goes, and where it does not

- **Rows that name a task in a token.** The Active list's agent rows and the
  Needs-you inbox, on all three surfaces. Both draw the identifier and then the
  slug, in the muted weight, outside the identifier's chip: the chip is the
  origin telling (gh#123), and the slug is a description of the work.
- **Branches, and so worktree paths and pull request head refs.**
- **Chat titles already carried the whole title** (`DispatchSpec::chat_title`,
  `gh#25 · D1 Prototype v1`) and every notice in `notify.rs` already carried it
  too. Neither needed a slug; a slug beside a title is the title said twice and
  worse the second time. The same goes for a board row, which draws the title in
  a column of its own.
- **Anywhere narrow, the slug goes and the identifier stays.** Measured on the
  TUI, whose sidebar caps at 32 columns: the inbox row renders `▲ gh#101
  review-page-loads`, and the Active row — whose right end belongs to gh#70's
  elapsed counter — renders `gh#101` alone. Whole or not at all, on that
  surface: half a slug reads as a broken word, and the branch on the sub-line
  now carries the words anyway.
- **Stack maps stay numbers.** `waiting on PR #11` names a location, and the
  task at that layer may not be a row on this board at all (gh#357).

### Not in this issue

The two viewports whose rows are laid out in pixels (gpui, SwiftUI) let the slug
*shrink* rather than disappear: it is the lower layout priority beside the name,
so a narrow panel truncates the description and never the identifier. That is
the same rule the terminal keeps by dropping the slug outright — the difference
is the medium, not the decision.

The Swift stopword list is a hand-port and has to stay in step with the Rust
one: a branch is named on the box, from the Rust list, and a phone stripping a
different set would render a different slug for the same task. There is no Swift
spec runner over these derivations, so that half rests on the Rust tests and on
review — the same standing gap gh#357 recorded.
