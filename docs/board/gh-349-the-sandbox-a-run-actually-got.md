# The sandbox a run actually got (gh#349)

A dispatched Codex agent finished its work, found it already on `main`, and
could not prove it:

> *"Note: remote main contains all merges. This sandbox could not update the
> local .git metadata because it is read-only, so the local checkout needs a
> pull outside the sandbox to synchronize."*

It reported that plainly instead of claiming a state it could not check, which
is the only reason this was ever legible. The board had dispatched it at
`workspace-write`; what it actually got was something under which `git fetch`
fails. Nothing on the board said so.

Two questions came out of that, and the answers turned out to be the same fact
read from opposite ends.

### What codex's `workspace-write` actually denies

Measured against the installed CLI, **codex-cli 0.147.0**, by running real
dispatched-shaped sessions and having them probe the filesystem:

| checkout | `<cwd>` writable | `.git` writable | `git commit` | `git fetch` |
| --- | --- | --- | --- | --- |
| main checkout (`.git` is a directory) | yes | **no** | **fails** | **fails** |
| linked worktree (`.git` is a pointer file) | yes | yes | ok | ok |

The rule behind it, isolated with a second probe in a main checkout on the
branch `main`:

| path | writable |
| --- | --- |
| `<cwd>/anything` | yes |
| `<cwd>/.git` | **no** |
| `<cwd>/sub/.git` | yes |
| `<other-repo>/.git` | yes |
| `<cwd>/.githooksish` | yes |

So the sandbox denies writes to **exactly `<cwd>/.git`** — not a nested `.git`,
not another repository's, not a `.git`-prefixed sibling — and nothing else about
the workspace. It has nothing to do with the branch name.

That is the whole of the report. A **main checkout** keeps its git dir at
`<cwd>/.git`, squarely under the deny rule: the agent can edit files and cannot
record the edit. A **linked worktree** has a pointer *file* there and its real
admin dir under the main checkout's `.git/worktrees/…`, outside the workspace —
so nothing it writes is ever under the rule, and it works. By accident, and only
because the paths happen not to overlap.

### Which means the escalation was pointed at the wrong case

`codex/mod.rs` widened `workspace-write` to `danger-full-access` whenever the
cwd was a linked worktree on a branch whose name contained `/`. Against the
table above, that condition fires on exactly the shape that already works, and
stays quiet for the main checkout that does not. It could not have prevented the
report, and it never did anything for the runs it fired on except turn the
sandbox off.

It also fired on **every** board dispatch into a worktree, because the default
branch template is `board/{identifier_lower}`. Every one of those agents had the
run device to itself, and the only record was a `WARN` in the journal.

The bug it was written for is real and is fixed. Codex ≤0.144.x derived a
malformed worktree mount for a slash-named branch and killed every command
before it started; on 0.147.0 a linked worktree on `board/gh-349-probe` runs,
writes, and commits under plain `workspace-write`.

### What changed

**1. The git dir is named as a writable root, and the sandbox stays on.**
`workspace-write`'s `sandboxPolicy` takes `writableRoots`, and putting the
checkout's resolved git dir in it lifts the deny. Verified over the *same wire
path the harness uses* — a `codex app-server` driven by hand through
`initialize` → `thread/start` → `turn/start`, in the main checkout that was
failing, twice:

| `turn/start` `sandboxPolicy` | result |
| --- | --- |
| `{workspaceWrite, networkAccess}` | `GIT_FETCH=FAIL`, no commit in the log |
| `… + writableRoots: ["<cwd>/.git"]` | `GITDIR_WRITE=ok`, `GIT_COMMIT=ok`, `GIT_FETCH=ok`, and the commit is in the log |

Sandbox still `workspace-write` in both. `git_writable_roots` resolves
it with plain filesystem reads and returns **both** the git dir and the common
dir: in a worktree those are different directories, a commit writes to the
first and a fetch to the second, and a list naming only one would fix `commit`
and leave `pull` broken.

This replaces a sandbox drop with a permission grant of two paths. It is the
part of this issue that actually fixes the reported failure.

**2. The escalation is gated on a codex old enough to need it.**
`WORKTREE_MOUNT_FIXED_IN = 0.147.0` — the last version verified broken is
0.144.x and the first verified fixed is 0.147.0, so 0.145 and 0.146 sit in a
band nobody measured and the threshold sits at the top of it. Being wrong here
costs a *visible* escalation on two patch releases; being wrong the other way
costs a run where no command can start.

A version this cannot read is **not** treated as old. The only thing the gate
controls is a sandbox drop, and a drop is an exception that has to be earned by
evidence rather than granted by the absence of it.

**3. Every harness states the sandbox it applied.**
`AgentEvent::Sandbox(SandboxReport)` — requested, effective, and why they differ
— emitted once per run before anything else, so it is the first thing a journal
records about a run. It is its own event rather than a field on `SessionStarted`
for the reason `ContextUsage` is: it is a different kind of fact, and the only
one here that can contradict the request the engine sent.

From there it follows the path `RunEvidence` already had: `RunJournal::sandbox`
reads the last run's line, `Runtime::run_sandbox` is the seam, the board
snapshots it onto the attempt in the same tick that records the commands, and
the review renders it above the effects. The terminal says

```
  ? this agent had full access to the box — workspace-write was requested, and …
```

and the desktop review carries the same sentence as a quiet band under the
verdict.

#### The other two runtimes were not telling the truth either

Once a surface displays the effective level, it has to be right for all three,
and it is not only Codex that was quietly ignoring the request:

- **Claude** takes no sandbox argument, and the adapter approves every
  `can_use_tool` it is asked (`handle_control_request`). Whatever level a
  dispatch names, what runs has the box.
- **opencode** is spawned as an ordinary child process with the run device's
  permissions, and its approvals are auto-answered the same way.

Both now report `danger-full-access` with the reason attached. That was already
true and was stated only in comments; a board that displayed the *requested*
level for those runs was reporting a guardrail it does not have. Sandboxing
either of them is not in this issue.

#### Why it is a caveat and not a finding

`AttemptReview::sandbox_note` deliberately stays out of `findings()`. Two of the
three runtimes are unsandboxed always, so routed through the findings list this
line would raise `Tone::Alarm` on nearly every review and the alarm would stop
meaning anything inside a week — the same argument `effect_chips` makes for
itself. It is a standing condition of the run, not something nobody accounted
for: a caveat on how much the rest of the screen is worth, placed where a
reviewer meets it before the evidence rather than after.

Full access is reported **even when it was requested**. "Nobody widened this" is
not the reassurance it sounds like when the level was `danger-full-access` from
the start.

#### Attempts from before this say nothing

The `run_sandbox` column is NULL on every existing row and stays NULL. Those runs
were never asked and their journals have moved on, so a review of one says the
level is unknown. Backfilling them from the level that was *requested* would
restore exactly the false record this column exists to replace.

`RunJournal::sandbox` reads the **last** run, not the loosest one ever seen: a
chat resumed after a CLI upgrade is on today's terms, and a warning that can
never be cleared is one nobody reads. The journal keeps every line, so the
history is there for anyone who wants it.

### Not in this issue

- **Sandboxing Claude and opencode.** They now say what they do; making them do
  less is a separate piece of work with its own blast radius.
- **The level on the board row.** The review is where a reader is weighing
  whether to trust an attempt's output, and it is the surface gh#349 named. The
  fact is on the attempt, so a row can read it later without new plumbing.
- **The phone.** `sandbox_note` is not a [`Finding`], so it is not part of the
  cross-language review *reading* and the iOS screen does not draw it. The
  fixture gains an ignored `sandbox` key on its inputs and nothing else moves —
  `review-spec.json` was regenerated and every case's reading is byte-identical.
  Carrying the caveat to the phone means a Swift half nobody can verify without
  the simulator, and it is a second surface's worth of work.
- **`approval_policy`.** Still `"never"` on all three runtimes. This issue makes
  the guardrail behind that choice observable; it does not revisit the choice.
