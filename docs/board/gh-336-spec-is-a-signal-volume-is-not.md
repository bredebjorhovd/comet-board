# Spec is a signal, volume is not — **deferred** (gh#336)

Re-homed from gh#281 before that tracking issue closed, so a deferred decision
would not die inside it. The question: **should the board ever decide to stack
on its own, and on what evidence?** The answer as it stands — *yes in shape,
not yet in fact* — and what would reopen it.

### Where stacking stands

Every entry point is explicit. An operator stacks with `--onto <task>` (§gh#285):
cut task B from task A's branch and aim B's pull request there — per dispatch,
never a route default. An agent stacks when the brief asks it to:
`dispatch --stack` (§gh#287), and `--decompose` (§gh#340) one level up.
Reading stacks is the other direction and default-on — the row surfaces, the
review band (§gh#389) and the merge rules exist because misreadings of stacks
that already existed needed fixing (gh#282, gh#283), not because anybody asked
the board to make one.

### Two candidate signals, and they are not equally safe

(Brede + review, 2026-08-11.)

**Spec is a good signal.** A ticket describing three separable concerns
genuinely is a stack, and the agent reading the ticket is the right thing to
notice it. The failure mode is mild: three pull requests where one would have
done.

**Volume is a weak one.** Task size correlates with stackability without
implying it — a large change can be indivisible, a small one can hold two clean
layers. A stack inferred from volume does not decompose along any real seam,
which is precisely the false positive the reading half is built to refuse:
`layer_of` is strict about layer names because a branch that merely reads like
a layer must stay a row of its own, and an inference that manufactures such
branches wholesale manufactures exactly what that strictness guards against.

### The shape worth trying first, when anything is tried

**The board suggests, the human confirms** — "this ticket names three concerns;
stack it?" at dispatch, one keypress either way. Inference without autonomy,
and reversible in a way an agent that has already opened five pull requests is
not.

Why not full autonomy even if the signal were trusted: an agent deciding on its
own to produce five pull requests where one was expected is a surprise worth
opting into — the reason both flags are off unless asked for — and a board that
inferred *dependency between tickets* well enough to chain `--onto` by itself
would sometimes cut a branch from the wrong parent. That is a wrong base, not a
wrong opinion; no review catches it cheaply.

### Why nothing landed here

Everything above reasons about a feature nobody has used yet, so the issue sets
a gate before building: watch a few real stacks land. Exactly one batch has —
the §gh#337 rig against board-scratch, four of them — and that is fixture
evidence about mechanics, not usage evidence about predicates. It proves the
board *reads* stacks correctly; it says nothing about when making one was right.

The watching needs no new machinery, because every stack lands somewhere
visible already: a row carries GitHub's own `stack` object, an attempt carries
`stacked_on` when `--onto` cut it from a sibling, and the review screen says
which layer a request is. When a handful have landed in anger, three questions
have answers:

1. did the layers match concerns the ticket actually named?
2. did any ticket whose text named one concern get stacked anyway, and did the
   layers hold up in review?
3. was volume ever the better predictor — a large indivisible change that
   spec-reading would have wrongly split?

If spec held and volume never helped, build the suggest flow; if the answers
are muddy, the predicate was never there and this stays closed. Either way the
suggestion must name the concerns it saw rather than count them — a prompt that
cannot say *which* three concerns it read is volume wearing spec's clothes.

### Where the finding landed instead

The decision lives today where it always has — with whoever passes the flag —
so the dispatcher-facing rule now says both halves: pass `--stack` when the
ticket's own text names several separable, dependent concerns, and never on
size alone. Updated together so they cannot drift:
`docs/agent-conventions.md`, the shipped skill
(`assets/skills/comet-board/SKILL.md`), and the `dispatch --stack` help in
`apps/board-cli`.

### Deliberately not here

- **No inference at dispatch** — no concern-counting heuristic, no model call,
  no route-level default that turns stacking on for a class of tasks.
- **No suggestion surface** — desktop, phone and CLI dispatch forms unchanged
  until the predicate above earns the checkbox.
- **No new counters.** Stacks already report themselves where they land; a
  census of zero events measures the deferral, not the feature.
