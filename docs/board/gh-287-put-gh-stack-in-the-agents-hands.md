# Put `gh stack` in the agent's hands — **done** (gh#287)

Stacks 6/9. Landed as `dispatch --stack`, the authoring half of
`crates/board/src/stacks.rs` (the branch convention and the block of brief), a
layer-aware `link_for` in the sync loop, a `gh stack` line in `doctor`, and the
extension install in the box wizard.

1/9 and 2/9 taught the board to *read* stacks. This is the other direction: one
dispatched agent decomposing its own task into layered pull requests, so that
five small layers are reviewed in parallel instead of one wall of diff. The
tooling is GitHub's — the `gh stack` extension, whose non-interactive flags and
"design the stack before you write it" guidance are already our conventions'
register — so the board's job here is small and specific: say *when*, name the
branches, and make sure the layers come back as one attempt's work.

### The four questions the issue was filed on

**Does `gh stack` survive the `gh` shim?** Yes, unchanged — gh#324 measured it
against a neutralised `GH_CONFIG_DIR`, and `docs/research/gh-stack-through-the-gh-shim.md`
is the write-up. Argv survives verbatim through `exec '<gh>' "$@"`, the
extension authenticates as `comet-board[bot]` off the shim-minted installation
token, and one mint covers a whole verb including its `git push`. Nothing in
`git_credentials.rs` was touched.

**Where does the extension install?** Box-level, because that is where `gh`
puts it: `~/.local/share/gh/extensions/gh-stack`, the XDG *data* dir.
`GH_CONFIG_DIR` — the only gh path the engine relocates per run — does not move
it, so per-run hermeticity was never on the table. So this issue carries a
one-time-per-box install, in three places rather than one, because the failure
it prevents is silent and arrives mid-task:

- the box wizard installs it beside `gh auth login`, where gh is already signed
  in;
- `comet-board doctor` prints a `gh stack` line saying whether this box has it;
- and the brief tells the agent to run `gh extension install github/gh-stack`
  itself if `gh` answers `unknown command "stack"`. gh#324 measured that install
  succeeding *through the shim on the minted token*, so an agent can repair a
  fresh box without anybody's login. That is the difference between a
  prerequisite and a dead run.

The doctor line never fails the report. Nothing in `routing.toml` says whether
a box stacks — the ask is per dispatch — so an absent extension is not a broken
board, it is a fact about what one flag would do here.

**Brief or skill?** Both, split by kind, exactly as gh#272 split them. The
`comet-board` skill and `docs/agent-conventions.md` carry *when* to stack,
because that is board policy: the dispatcher's rule, one paragraph, next to the
other dispatch rules. The brief carries *how*, because the how is per dispatch —
it names this attempt's own branch, this dispatch's base, and the commands in
order. Upstream's own gh-stack skill is the deeper reference for technique and
is installed the way any gh skill is; nothing here vendors a copy of it, which
would be a third place to go stale.

**When should an agent stack?** Only when the brief asks — `dispatch --stack`,
off by default, per dispatch rather than per route. An agent opening five pull
requests where one was expected is a surprise worth opting into, and how many
concerns a task holds is a property of the work rather than of the class of
work a route describes. A size threshold can come later, once this has been
watched.

### The branch convention, and the shape the issue asked for that git cannot store

The sharp edge was always naming. The board names one branch per attempt and
links a pull request to a task by that name; an agent-authored stack creates
branches the board never named, and unless the names *say* whose they are, every
layer above the first arrives as a pull request belonging to nobody — imported
as a row of its own, dispatchable, reviewed by no chat.

The issue proposed `board/gh-12-widget/2`. **Git cannot store it.** A ref is a
path, so `refs/heads/board/gh-12-widget` being a file forbids
`refs/heads/board/gh-12-widget/2` from being a directory — and the bottom layer
*is* the attempt branch, which is the whole point of the convention, so the
nesting separator is the one shape unavailable. The convention is a suffix on
the last segment instead:

```
board/gh-12-widget      layer 1 — the attempt's own branch, never renamed
board/gh-12-widget-2    layer 2
board/gh-12-widget-3    layer 3
```

`stacks::layer_of` reads it back, and it is strict on purpose — digits only, no
leading zero, never below 2 — because a false positive swallows a real pull
request into somebody else's attempt. Two further guards stand behind it: the
match is scoped to one repository first (`board/gh-28-x-2` in repo `x-2` is not
a layer of `board/gh-28-x` in repo `x`), and a branch is only read as a layer
when GitHub's own `stack` object is on the pull request. A branch that merely
reads like a layer stays a row of its own.

### What linking does with a stack

`link_for` replaces the old "first pull request whose head is an attempt
branch". It collects every layer, one representative per branch (the newest
request on it), and answers three things:

- **the linked pull request is the bottom layer** — the branch the board named
  and the attempt recorded, and so the one `authoring_attempt` and `adopt` can
  still match an attempt to;
- **`pr_open` is "any layer open"**, so the GC holds the checkout while the
  layers above are still being written in. The bottom of a stack merges first,
  and it used to take the row's `open` with it;
- **`pr_merged` is "they all landed"**. Merging the bottom finishes nothing; a
  task closed on it would close its issue and free its worktree with four
  layers outstanding. A layer closed *without* merging counts as neither — it is
  work the agent withdrew, and holding `merged` down on it would leave a stack
  that can never read as finished.

For an unstacked attempt every one of those collapses to what it was: one
representative, its own `open`, its own `merged`.

### Not in this issue

**Review delivery is still one pull request per row.** Feedback on the bottom
layer reaches the authoring chat exactly as before; feedback on the layers above
it does not reach anything yet — `deliver_review_for` resolves one pull request
per task and keeps one delivery watermark per task, and per-layer delivery is a
change to that machinery rather than to this one. It is 7/9's (gh#288) to make,
which is also where retargets stop defeating the cost gate. Until then a stacked
dispatch is a better *review* shape and not yet a better *relay* shape, and the
conventions say so rather than leaving it to be discovered.

**The board still does not stack its own dispatches.** 4/9 (gh#285) is the other
direction — dispatching onto a sibling's branch — and shares only the flag's
neighbourhood, not its code.
