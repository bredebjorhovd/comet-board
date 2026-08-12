# The brief never asked for claims — **done** (gh#339)

Live on the box: settled attempts reaching the review window with nothing in the
middle of them. No claims, so no count, no evidence chips, and — the part that
matters — no remainder, because there is nothing to compute one against. The
screen degrades to a worse GitHub.

The ticket asked four questions in order. The answers, off the box's own
`board.db` and the run journals beside it:

1. **Are agents running `claim` at all?** No. Twenty-three settled attempts,
   every one `claude-code`, `claims_at` NULL on all of them. Reconstructing each
   chat's text from its journal: **zero** ```` ```claims ```` fences written and
   **zero** `comet-board claim` invocations among every command those runs ran.
2. **Did the instruction reach them?** The skill was installed and discoverable
   for all twenty-three. So the answer is worse than gh#272's: agents that *had*
   the channel did not use it.
3. **Was a block refused?** No. `claims_error` is NULL on every row — there was
   nothing to refuse.
4. **Is it read-side?** No. `harvest`, `find_block`, `RunJournal::final_text`
   and the settle-time call in `SyncEngine::reconcile` all do what they say; the
   review keeps "never answered" and "claimed nothing" apart all the way to the
   render. Nothing in that path was ever reached.

### Why an installed skill was not enough

The contract had three channels to an agent and none of them was the one that
always arrives.

- **The skill is discovered.** Claude Code reads it when the agent decides it is
  relevant, and an agent handed a ticket, a branch and a finish sequence has no
  reason to decide that. Codex has no skill mechanism at all.
- **The instruction file (§gh#272) is a standing rule in a file**, competing
  with everything else in that file, and it landed after every run above.
- **The brief is neither.** It is the one text the board hands to every
  dispatched agent of every runtime, on every dispatch, and until now it asked
  for a commit, a push and a pull request and never mentioned claims.

Agents did exactly what the brief asked. So the ask moves to where the work is
asked for: `crate::claims::brief` is appended by `resolve_prompt` after
interpolation, on `pr_base_line`'s rule — a route's own `prompt` is somebody's
wording for the *task*, and the review contract is a fact about being dispatched
at all. Unconditional, and last, because claiming is the last thing an agent
does.

### The refusal waiting at the end of that road

Fixing the ask alone would have replaced silence with a wall of refusals.

The brief opens with the task's **identifier** (`gh#339`). Every verb of the
review contract — `claim`, `review`, `verdict` — resolved `--task` against the
**id** column (`gh:owner/repo#339`). Nothing in a dispatched run exports the id:
no environment variable carries it, and the two are different strings. An agent
that followed the skill to the letter would have been told `gh#339 is not on the
board` — which reads as the board having lost the row, not as the wrong
spelling, and has no repair in it.

Both halves land together:

- `dispatch::task_by_reference` — id first and whole, then identifier,
  case-insensitively; an identifier two rows answer to is refused rather than
  guessed, and the message names the id that settles it. `stack_parent` already
  had this logic inline for `--onto` and now shares it, so `--task` and `--onto`
  cannot drift apart on what a task may be called.
- The brief prints the **id** beside the verb anyway. Both spellings work, but
  the one printed is the one that can never be ambiguous.

Resolution happens at the door, not in the payload: `AttemptReview.task_id` is
the canonical id whichever spelling was typed.

`{task_id}` joins the prompt variables for a route that would rather place it
itself.

### What is deliberately not here

**No gate.** The instruction-file block has `agent_instructions` because it
writes into somebody's config dir and has to be undoable. The brief is the
board's own message to an agent it started, and the contract is what the board
dispatched for.

**No duplicate check.** A route whose custom prompt already mentions claims gets
the paragraph twice. That is cheaper than an attempt that claims nothing, and
detecting it would mean the board reading a template for meaning.

**Nothing read-side changed.** The remainder, the harvest and the two-way
distinction between an unanswered contract and an empty answer were all correct
and untouched.

### Where to watch for it working

The verification that matters is the next settled attempt on the box:
`comet-board review --task <id> --json` with a non-null `claims_at` on it.

**On the box, and nowhere else.** `resolve_prompt` is comet-board's, and
`herdr-board` is a different binary out of a different repo (`~/dev/herdr-board`,
installed at `~/.local/bin/herdr-board`) with its own brief. A dispatch from
there — every worktree under `~/.herdr/worktrees/`, including the one this was
written in — does not pass through this code and will not carry the paragraph.
Watching a herdr dispatch for the ask to appear and concluding the fix did not
work is worse than the bug it fixes, because it looks like evidence. If the ask
is wanted there too, it is herdr-board's own change to make.
