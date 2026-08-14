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

### What codex's `workspace-write` actually grants

Measured against the installed CLI, **codex-cli 0.147.0**, by driving real
sessions and having them probe the filesystem.

`workspace-write` grants the **workspace, `/tmp` and `$TMPDIR`**, and on top of
that denies **exactly `<cwd>/.git`**. The deny is narrow and it beats the
grants — isolated with a probe in a main checkout on the branch `main`:

| path | writable |
| --- | --- |
| `<cwd>/anything` | yes |
| `<cwd>/.git` | **no** |
| `<cwd>/sub/.git` | yes |
| `<other-repo>/.git` | yes (it is under `/tmp`; see below) |
| `<cwd>/.githooksish` | yes |

Not a nested `.git`, not a `.git`-prefixed sibling, and nothing to do with the
branch name.

**A correction, because it changes the conclusion.** The first round of this
measurement ran in a fixture under `/private/tmp` and showed a linked worktree
committing happily — from which the obvious reading was that worktrees "work by
accident", their git dir being outside the deny. That reading was an artifact of
the fixture's location: `/tmp` is granted wholesale, so *everything* there is
writable. Repeated on a checkout under `$HOME`, where board worktrees actually
live:

| checkout (under `$HOME`) | `git add` | `commit` | `fetch` | `merge` |
| --- | --- | --- | --- | --- |
| main checkout (`.git` is a directory) | ok | **fails** | **fails** | **fails** |
| linked worktree (`.git` is a pointer file) | **fails** | **fails** | **fails** | **fails** |

Both shapes lose their git dir, for two different reasons. A **main checkout**
keeps it at `<cwd>/.git` — inside the workspace and explicitly denied. A
**linked worktree** keeps a pointer file there and its real admin dir under the
main checkout's `.git/worktrees/…` — outside the workspace, so never granted at
all; a worktree run cannot even stage a file.

### Which means the escalation was load-bearing, and pointed at the wrong thing

`codex/mod.rs` widened `workspace-write` to `danger-full-access` whenever the cwd
was a linked worktree on a branch whose name contained `/`. The board's branch
template is `board/{identifier_lower}`, so that fired on **every** board dispatch
into a worktree — and, given the table above, was the only reason those runs
could commit at all. Every one of those agents had the run device to itself, and
the only record was a `WARN` in the journal.

The condition was still the wrong one twice over: it never fired for the main
checkout that produced the report, and a worktree on a branch *without* a slash
got no escalation and no writable git dir either — it simply could not commit.

The bug it was written for is real and is fixed. Codex ≤0.144.x derived a
malformed worktree mount for a slash-named branch and killed every command
before it started; on 0.147.0 that is gone.

This is why the two changes below ship together. Removing the escalation on its
own would have broken every worktree dispatch on the box.

### What changed

**1. The git metadata is named as writable roots, and the sandbox stays on.**
`workspace-write`'s `sandboxPolicy` takes `writableRoots`. Verified over the
*same wire path the harness uses* — a `codex app-server` driven by hand through
`initialize` → `thread/start` → `turn/start`:

| `turn/start` `sandboxPolicy` | result |
| --- | --- |
| `{workspaceWrite, networkAccess}` | `git add`/`commit`/`fetch` all fail, nothing in the log |
| `… + writableRoots` | `add`, `commit`, `fetch`, `merge`, `rebase`, `branch -f`, `checkout -b`, `stash`, `stash pop`, reflogs — all ok, and the commit is in the log |

Sandbox still `workspace-write` in both. `git_writable_roots` resolves the paths
with plain filesystem reads — a sandbox decision that depended on a subprocess
succeeding inside the thing being sandboxed would fail in the interesting case.

#### What the roots grant, and the one thing they must not

The list is deliberately asymmetric, because the two directories are not the
same kind of thing:

- **The git dir, entire.** The run's own state: index, `HEAD`, `FETCH_HEAD`,
  `COMMIT_EDITMSG`, the rebase and merge scratch dirs. There is no useful subset
  — and for a main checkout it is `<cwd>/.git`, sitting in the workspace the
  agent already writes to freely, so whole-directory access is the same trust
  level as the build scripts beside it.
- **`objects/`, `refs/`, `logs/` under the common dir**, and only when that is a
  *different* directory — i.e. a linked worktree. (`packed-refs` was a fourth
  here and is gone: it is a *file*, and on Linux a file writable root fails the
  sandbox setup and takes every command in the run with it — §gh#394.)

That second case is the one worth stating plainly, because on this box it is
every board dispatch:

```
$ cd ~/.herdr/worktrees/comet-board/board-gh-349-comet-board
$ git rev-parse --git-dir --git-common-dir
/Users/brede/dev/comet-board/.git/worktrees/board-gh-349-comet-board
/Users/brede/dev/comet-board/.git
```

The common dir is the **operator's own working repository**, not a copy and not
the workspace. Granting it whole would hand a dispatched agent `hooks/` and
`config` there — and a `.git/hooks/pre-commit` written into that repository runs
*on the operator's machine, outside any sandbox*, the next time they commit.
`config` reaches the same place through `core.pager` or an alias. A sandboxed
run would be a documented way out of the sandbox, and "the deny lifts, the
sandbox stays on" would be true of the sentence and false of the situation.

So the subpaths are named instead of the directory. Measured on a worktree
under `$HOME` with exactly those (then four; `packed-refs` has since been
dropped — §gh#394):

| | narrowed roots |
| --- | --- |
| `add`, `commit`, `fetch`, `merge`, `rebase`, `branch -f`, `checkout -b`, `stash`, `stash pop`, reflog | **ok** |
| write `<common>/hooks/pre-commit` | **denied** |
| write `<common>/config` | **denied** |
| write `<common>/` at all | **denied** |
| `$HOME` | **denied** |

Two things genuinely stop working, and both are the operator's own maintenance
rather than an agent's work: `git gc` and `git pack-refs`, which write `gc.log`,
`gc.pid` and `packed-refs` at the shared root. The board's git identity is
unaffected — it stamps `GIT_AUTHOR_*` on the harness child (gh#107) rather than
writing `config` in the checkout — but an agent that tries to `git config
--local` something in a worktree will now be refused, where before it was not.

This replaces a sandbox drop with a permission grant of a handful of paths, all
but one of them leaves. It is the part of this issue that fixes the reported
failure.

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

The cost of that silence is worth naming, because it lands exactly on the case
this issue newly grants. A workspace-write dispatch that is *not* escalated has
`requested == effective`, so `note()` returns `None` and the review says nothing
— which is right (a band on every review is a band nobody reads) and does mean
the review is not where anyone will learn what the writable roots permit. This
document is. If the roots ever widen — the shared root, `hooks/`, `config` — the
section above is the thing to change, and a run that got them would still show a
clean review.

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
