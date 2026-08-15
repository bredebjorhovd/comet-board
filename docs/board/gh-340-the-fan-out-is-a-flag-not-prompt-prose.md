# The fan-out is a flag, not prompt prose — **done** (gh#340)

Landed as `dispatch --decompose` / `retry --decompose`: `--stack`'s twin one
level up, carried through the same seam (`DispatchOverrides`, the RPC's
`DispatchTaskParams`, the CLI), and like it the brief is the only thing it
changes — `decompose_brief` in `crates/board/src/dispatch.rs`, appended by
`resolve_prompt` where the stack block goes.

Live on the box, v0.4.0: an agent does not spin work out to other agents
unless the prompt spells it out — "invoke the skill and dispatch these as
separate tasks", typed into the ticket by someone who knows the tool. The
issue offered three answers, roughly in order of cost: say it in every brief,
make it a flag, or call it documentation. What landed is the flag, and the
reasons the other two lost are worth keeping.

### Why not a line in every brief

The unconditional line was the issue's cheapest option, and it was written
against v0.4.0, where Codex had no channel at all. That gap has since closed:
the conventions block (gh#272) is written into the instruction file each
runtime reads on its own — `CLAUDE.md`, `AGENTS.md` — on every dispatch, and
it already carries the standing rule this line would repeat: work you delegate
goes through the board, in-chat subagents are for reading, anything that lands
a commit is a ticket. Repeating a standing rule in every brief pays rent
forever and buys nothing the conventions do not.

And the *authorizing* form of the line — "if this needs more than one agent,
open tickets" — would collide with the bullet two down in the same block:
**nothing is dispatched speculatively; a human keypress or an explicit
instruction is what releases work.** A brief that hands every agent a standing
authorization to fan out is that rule repealed by side door, on every dispatch,
for every task. The rule is right — a dispatch starts a real agent that bills
somebody — so the authorization has to be per task, which is to say: a flag.

### Why not nothing

Because "say it in the prompt" is exactly the state the issue was filed on.
It works for whoever knows the tool and is invisible to anyone else; a flag is
in `--help`, in the skill's verb table, and in the RPC surface a frontend can
put a checkbox on. gh#287 already made this trade for stacks and its reasoning
transfers whole: an agent deciding on its own to open five tickets is a
surprise worth opting into, so the deciding stays a human's — the flag is how
it is said cheaply.

### What the brief block says

The conventions carry the *why* (through the board, what a ticket buys); the
brief carries the per-dispatch *how*, on `stack_brief`'s pattern — the facts
the agent cannot guess and the honest way out:

- the command, with the task's own repo interpolated (`comet-board new
  "<piece>" --repo owner/widget --body - --dispatch`) so pieces land beside
  the parent issue on a box that watches several repos, and bare for a Linear
  task, where `[defaults] new_source` answers;
- write the body for a stranger — it is everything the piece's agent will
  know — and say it is part of this task, so the tracker shows the fan-out;
- provenance is automatic (`COMET_BOARD_CHAT_ID`), the chat is prompted when
  pieces settle or block, and after releasing you wait or say plainly that
  you are leaving them running;
- **keep a slice**: the part that needed the whole picture stays in this chat,
  committed and pushed as usual. This is not sentiment — the attempt still
  settles on a pull request or pushed commits (§settle-logic), so a chat that
  releases everything and pushes nothing is `Why::NoArtifacts` forever: a row
  in `working` until the clock cap ends it as failed. The brief says so
  outright rather than letting a thorough delegator discover it;
- and the honesty clause, verbatim in spirit from the stack block: if the work
  does not split into pieces a stranger could carry, do it all here and say in
  the pull request description why it did not.

### `--stack` and `--decompose` are refused together

Both are decomposition asks in different dimensions — a stack layers one
attempt's own pull requests, a decomposition releases pieces to other agents —
and a brief carrying both blocks reads as "split this twice" with no rule for
which cut comes first. Refused in `build_spec` (and by clap before that), like
`--base` with `--onto`: the pair is a contradiction, not a combination.

### Not in this issue

**The board still does not infer that a task is too big.** That is gh#336's
question (for stacks) and this flag's obvious next temptation; both want a
board that has watched dispatched work long enough to have a size prior, and
neither should be guessed at from here.

**No frontend toggle yet.** The RPC key is there (`decompose`, absent unless
true, so an engine that predates it is sent nothing to ignore); the desktop
and phone dispatch surfaces can grow the checkbox when the CLI form has been
used enough to trust the wording.
