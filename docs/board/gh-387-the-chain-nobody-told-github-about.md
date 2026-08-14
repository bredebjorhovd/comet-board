# The chain nobody told GitHub about — **done** (gh#387)

Found by dispatching a real three-layer chain on `Florin-AS/orion-productmapping`
(2026-08-13, board 0.6.0), which is the first time anything had.

`--onto` did exactly what it documents. Each branch was cut from the branch
below it — `git merge-base --is-ancestor` confirms it — and each pull request
opened against that branch instead of trunk:

```
PR 47  base: main                              (layer 1)
PR 48  base: board/gh-44-packages-kind-decided (layer 2)
PR 50  base: board/gh-45-auto-promote-bug      (layer 3)
```

And **no stack existed**:

```
GET /repos/Florin-AS/orion-productmapping/stacks     → []
graphql pullRequest(48) { stack, stackEntry }        → null, null
```

So the dependency lived in the branch bases and nowhere else — not in GitHub's
model, not in the board's row. Running `gh stack link 47 48` by hand fixed
everything downstream with no other change: the moment the stack object existed,
every layer's row carried the whole chain with its own position.

That is the shape of this bug. The grouping code (gh#283), the landing rules
(gh#282), the propagation (gh#289), the merge key (gh#290) were all correct and
all keyed on an object no dispatch ever created. **A board where every stacks
feature works and no dispatch ever creates a stack is the worst version of
this**, because nothing is broken enough to notice.

Not covered by gh#337, whose rig builds its stack with `gh stack` and tests the
`--stack` path (gh#287). `--onto` (gh#285) produces a different shape — separate
tasks, separate attempts, chained bases — and it is that shape which silently
produced no stack.

### Why not at dispatch

The issue asked for `--onto` to call `gh stack link` "once the pull request
exists", and that clause is the whole design constraint: at dispatch there is no
pull request. There is a task, a branch and a brief. The chain becomes
stackable at some unpredictable point afterwards — when the last agent runs `gh
pr create` — so the board watches for it the way it watches for everything else,
on the cycle that has just polled the pull requests.

`SyncEngine::link_dispatched_stacks` runs straight after `poll_github`, which is
the poll that linked those pull requests to their rows.

### Why REST and not `gh stack link`

The extension was the obvious route — gh#324 measured that it works through the
board's `gh` shim on a minted installation token — but the board is not an
agent. It has a REST client, an App credential and no PATH of its own, and
`gh-stack` is a box-level install that a fresh box has not got.

GitHub has the endpoints:

```
POST /repos/{owner}/{repo}/stacks           {"pull_requests": [47, 48]}   → 201
POST /repos/{owner}/{repo}/stacks/{n}/add   {"pull_requests": [50]}       → 200
```

Both answer on the `X-GitHub-Api-Version: 2022-11-28` the board already sends —
verified against a live repository before this was written, with a two-item
minimum enforced by GitHub itself:

```
$ gh api --method POST -H "X-GitHub-Api-Version: 2022-11-28" \
    /repos/bredebjorhovd/comet-board/stacks -f 'pull_requests[]'
Invalid property /pull_requests: 2 items required; only 0 were supplied. (422)
```

So `Github::create_stack` / `Github::add_to_stack`, beside the read half that
has been parsing the `stack` object since gh#282.

### The plan is made from board rows

`stacks::unlinked` is three pure functions over the task list, and the sweep that
executes them holds no policy at all:

- **`chains`** — every chain `--onto` built, bottom first, from
  `attempts.stacked_on` and **only** from it. Never GitHub's own stack object:
  reading that back in here would have the board proposing to restack pull
  requests somebody else's tool arranged, which is a second opinion nobody asked
  for rather than a repair. A fan (two dispatches cut from one sibling) is two
  chains, because GitHub stacks are linear.
- **`segments`** — the runs of a chain GitHub would actually accept. Every
  reason it might not is a reason to *narrow* rather than to refuse: a layer with
  no pull request yet, a layer that has landed, a layer in another repository, a
  base ref that is not the layer below's head ref. Split there and keep what
  survives.
- **`plan`** — nothing stacked yet is a `Create` of the whole run; a stacked
  prefix with unstacked layers above it is an `Add` of the layers above.
  Anything else — two stack numbers in one run, a stacked layer above an
  unstacked one — is `None`. The board may complete a chain it cut; it may not
  rearrange one it did not.

`Create` then `Add` is the sequence the issue watched: the second `--onto`
creates, the third and every one after it joins.

### Asking once, and giving up

The sweep is a write on a loop, so the interesting question is when to shut up.
`stacks::Asked`, stored under `meta::stack_asked`, is both the "asked already"
mark and the budget:

- **A request GitHub took is never sent twice.** The poll that would prove it
  landed can be a whole cycle away, and the board must not fill that window with
  retries.
- **A refused request is sent `LINK_TRIES` times and then dropped.** The honest
  failures here are races that heal in seconds; the dishonest ones — stacks off
  for the repository, a credential without the permission, a preview that has
  moved — do not heal at all, and the sweep runs every cycle. gh#378's rule,
  one feature along.
- **Keyed on what would be sent, not on a clock.** A chain that grows a layer is
  a different request and gets a fresh budget, which is what stops "gave up on
  this shape of the chain" from becoming "gave up on the chain".

### What this deliberately does not add

**A second record of the layer relationship.** The issue raised it as worth
deciding, and the answer is that the board has held it since gh#285:
`attempts.stacked_on` is an attempt id, written with the insert, and it survives
the parent merging and its branch being deleted. That is exactly why this could
be planned from board rows alone — and why a retry, or any question about
ordering, already had something to consult in the window before the pull requests
exist. Nothing was missing there. What was missing was the call.
