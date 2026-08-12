# The dependency no page admitted to — **done** (gh#335)

§gh#287 put `gh stack` in three places, and every one of them is *box*-shaped:
the setup wizard installs it beside `gh auth login`, `comet-board doctor` prints
a line about it, and the dispatch brief tells the agent to install it itself when
`gh` answers `unknown command "stack"`. Three mechanisms, none of them a
sentence a person reads before the fact. `docs/teammate.md` — the five-step page
for putting somebody on the box — did not mention it, and neither did
`docs/macos-install.md`, which is where somebody sets up their own machine.

Nothing was broken by that, which is exactly why it lasted. The self-repair path
works: gh#324 measured `gh extension install` succeeding through the board's own
`gh` shim on a minted installation token, so an agent handed `--stack` on a bare
box installs the extension and carries on. What it costs is the opening minutes
of a run that is billed and capped, spent on tooling, while the operator watches
a dispatch that looks like it is thinking.

### What a person now reads

- **`docs/teammate.md`** gains a section after the five steps rather than a
  sixth step, because it is not per person: `gh` installs extensions into
  `~/.local/share/gh/extensions/`, so one command covers the box, every slot on
  it and everybody who will ever be added to it. Adding a teammate never needs it
  done again, and a step that is done once for the machine standing in a
  five-step per-person sequence would be read as one that is not.
- **`docs/macos-install.md`** gains the other case, the quiet one: a teammate on
  their own laptop who opens a stack the board produced. Without the extension
  `gh pr list` shows five pull requests with nothing saying which sits under
  which, and `gh stack view` is an unknown command. Their machine is not the
  box's, and neither install substitutes for the other — the page says which one
  it is talking about.

### The doctor line, and the state it cannot be in

The issue asked whether the `gh stack` check should be louder "on a box that has
a route with stacking enabled". **There is no such box.** Stacking is asked for
per dispatch (`--stack`); `routing.toml` has no stacking key, and the flag is not
kept on the attempt either — it shapes the brief and is gone. So there is no
configured intent to read.

What there is, is history. A row whose `pr_stack_number` survived a poll is a
stack this board produced or adopted, and `Db::stacked_task_count` is the whole
of the evidence. The check now says one of two things when the extension is
absent:

- **no stacked rows** — what it said before: an absent extension is a fact about
  what one flag would do here, not a fault.
- **stacked rows** — a box already doing the thing it lacks the tool for. The
  next `--stack` run pays for the install inside its own billed minutes, and the
  operator cannot run `gh stack view` on the chains the board is already holding.

It still never FAILs, in either case, and that is the deliberate half. The agent
repairs it in place; a FAIL an operator is right to ignore is worse than no line
at all, because it is the one that teaches them to skim the red ones. The change
is which sentence they read, not what colour it is.

### Not in this issue

**Nothing installs it on a box the wizard never ran on.** `doctor` reports and
the agent self-repairs, which is the pair §gh#287 chose; a `comet-board` verb
that shells out to `gh extension install` would be a fourth place to keep in
step with the other three. The wizard is where box-level installs live.
