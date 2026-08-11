# Dispatch onto a sibling — **done** (gh#285)

Stacks 4/9, on top of gh#284. The board could *read* stacks (1/9, 2/9) and could
aim a pull request at a non-default base (3/9). It could not make one: every
dispatch branched from the same place, because where a dispatch branches from
was a **route** answer.

`base` lives in `routing.toml`, one string for every task the route matches.
That is the right shape for "this repo's work starts at `origin/HEAD`" and the
wrong shape for the thing a stack is made of — task B cut from task A's branch,
which is a fact about one release and about nothing a config file could have
known when it was written. There was no way to say it. That is what this adds.

### The change

`DispatchOverrides` gains two fields, and `build_spec` reads the first:

- **`base`** — cut this dispatch from that branch instead of the route's key.
  The escape hatch: a release branch, a colleague's branch, anything no task on
  the board holds.
- **`onto`** — stack this dispatch on that *task*. `stack_parent` resolves it to
  the attempt holding the branch, and the branch becomes the base.

`onto` is the spelling anybody actually uses. A task is what the operator is
looking at; the branch is an implementation detail of the attempt on it. Both
reach the engine over `DispatchTask {onto?, base?}` and the CLI as
`dispatch --onto <task>` / `--base <branch>`. Naming both is refused rather than
ranked: they are two spellings of one decision, and quietly obeying one of them
is how a dispatch ends up cut from a branch nobody asked for.

Downstream, nothing new happens. The base is one value doing two jobs — where
the branch is cut and where the pull request is aimed — and it stays one, which
is exactly what gh#284 was for: a layer cut from its sibling whose request
targeted trunk would carry the sibling's commits. `Repos::resolve_base` already
fetched any named branch from origin, so a sibling's branch needs no new
machinery there; `base_sha` is still stamped from the checkout's own HEAD, which
for a stacked layer is the parent's tip.

### The parent has to have pushed

`resolve_base` fetches before it cuts, and a fetch that fails refuses the
dispatch rather than falling back (gh#67). Applied to a sibling's branch that
rule says: **the parent must be on origin.** An unpushed parent branch is not,
the fetch fails, and the release is refused.

That refusal is kept, deliberately. The fallback would be to cut from trunk, and
the operator would get a "stacked" layer whose diff is the whole feature — the
exact failure gh#284 existed to prevent, arriving silently. What changed is only
the *message*, which now names the case:

> could not fetch `board/gh-12-widget` from origin in … — refusing to branch
> from a possibly stale local checkout. If `board/gh-12-widget` is another
> attempt's branch, that attempt has not pushed it yet: a dispatch stacks on
> what is on origin, and there is nothing there to cut from until the parent
> pushes.

In practice this means stacking on a task in `review` (its agent pushed and
opened a request) is routine, and stacking on one that is minutes into `working`
may not be yet. Waiting is the answer, and the message says so.

### The two open questions

**Should the child's row record the parent attempt, or just the branch?** The
attempt. `attempts.stacked_on` is a new nullable column holding the parent
attempt's id, written with the insert — the parent is decided before anything is
created, so a row that exists at all knows what it was cut from.

Branch-string equality was the alternative and it is fragile in the one
direction that matters: merging the parent *deletes its branch*, and 5/9 (GC of
dependents) and 8/9 (feedback fan-out) both need the edge precisely then. Both
are asking about a run — "is a child still building on this checkout", "who else
should hear this review" — and a run is an attempt, not a name.

**Can the reuse path pair an old child branch with a new base?** Yes, and it now
says so. `create_worktree_on` reuses an existing branch as it stands and never
re-points it, because a retry has to land on the previous attempt's commits
rather than rebase them onto a newer base. So a dispatch naming a base for a
branch the task already holds gets the old branch and its old starting point.

Warned, not refused: the ordinary case is a retry of an already-stacked task,
whose branch is on the right parent already, and refusing that would make a
stacked task un-retryable. The board logs which attempt holds the branch and
that the base is not what this dispatch is cut from, so the operator does not
read the base back off their own command line.

### Not in this issue

The frontends. `DispatchTask` is the contract and both the CLI and the RPC carry
`onto`, but the desktop picker and the TUI still send neither — the gesture
("stack a follow-up on this", from a row that already has a running or review
attempt) is a UX surface with its own list of candidate parents, and it belongs
with the rest of the stacking UX rather than bolted to the runtime/model/account
picker. Everything under it is in place for that to be a keypress.
