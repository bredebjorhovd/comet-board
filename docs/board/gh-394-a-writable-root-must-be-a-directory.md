# A writable root must be a directory — **done** (gh#394)

A codex attempt on the box came up, and then could not run a single command.
Not `git`, not the build, not `pwd` from `/tmp`. Every one of them failed the
same way, before it started:

```
Can't mkdir parents for …/tally/.git/packed-refs/.git: Not a directory
```

The agent stopped and wrote out what was happening in plain prose instead of
working around it, which is the only reason anyone found out. Attempt burned,
task re-released on claude-code.

Two separate faults, and the second is the one that made the first expensive.

### 1. We handed codex a file and called it a writable root

gh#349 gave a dispatched agent write access to the git metadata its checkout
needs, as a narrow list rather than the shared root — `hooks/` and `config` in
the operator's own repository are how a sandboxed run gets out of the sandbox,
so the grant names leaves. Four of them:

```rust
const SHARED_GIT_WRITABLES: &[&str] = &["objects", "refs", "logs", "packed-refs"];
```

`packed-refs` is a **file**. The other three are directories.

That list was measured working on macOS, where seatbelt takes a file root
without complaint. The box that runs the dispatches is Linux, and there a
writable root is a mount: codex treats each one as a directory it may have to
create and reach *through*, and the derived path in the error message says so —
it is resolving a `.git` **under** `<common>/packed-refs`, which cannot exist
because `packed-refs` is not a directory. The failure is in sandbox setup, so it
lands before the agent's first command rather than on the first `git` command,
which is why `pwd` from `/tmp` failed too.

Every attempt on the box runs in a linked worktree, every worktree's common dir
is the operator's packed repository, so this was every codex dispatch on that
box. opencode and claude-code on the identical worktrees were fine — they apply
no sandbox at all (gh#349 §"the other two runtimes"), so they never named a
root.

The fix is the invariant, not the one name: `mountable_root` keeps a candidate
only if it is a directory or is absent (codex creates an absent root, as a
directory — the right shape for every leaf that is left, and a repository with
no reflogs yet has no `logs/`). Anything that exists in another shape is
dropped, because one unmountable root is not a narrower sandbox, it is a dead
run. `packed-refs` leaves the list on the same grounds.

What that costs: deleting an **already-packed** ref — `git branch -d` of a ref
that has been packed, `fetch --prune` of a packed remote ref. It costs nothing
on the path an attempt uses. A commit, a fetch, a branch create and a reflog all
write *loose* files under `refs/` and `logs/`, both still granted; a loose ref
shadows its packed entry, so a branch whose ref is packed is still committable.
`git gc` and `git pack-refs` remain unavailable, as they already were —
`gc.log` and `gc.pid` at the shared root were never granted, and repository
maintenance is the operator's, on their own clone.

### 2. A sandbox that fails setup left the row reading `working`

The first fault is a bug in a list. This one is the reason it cost an attempt
and a human's afternoon.

Nothing died. The app server was up, the thread was live, the model was
answering — it simply could not execute anything. So the agent could not push,
could not open a pull request, could not claim, **and could not signal blocked
either**, because signalling blocked goes through a run that ends. The board saw
a live chat with a live run and reported exactly that: `working`,
indistinguishable from an attempt making progress. It surfaced only because a
human read the chat.

A run that cannot start a command is over. `sandbox_setup_failure` reads a
completed `commandExecution` and ends the run when the sandbox itself is what
failed:

- the command must have **failed** (status or non-zero exit), and
- its output must carry a wrapper's own setup error — the `mkdir parents` line
  above, `bwrap:`, `failed to set up sandbox` and the few spellings around it.

A *denied write* is not on that list and never ends a run: a denied write is the
sandbox working, and the run carries on. A build log or a `grep` that merely
mentions one of those strings exits 0 and is ignored.

The failed command is journaled as a command first — a reviewer sees what was
attempted — and then the run emits `Error` and `Done { Errored }`, which is what
the rest of the board already knows how to read: `SessionStatus::Errored` →
`AgentStatus::Blocked`, and `settled::decide` returns `StayLive(Why::Errored)`,
so the attempt stays live and retryable with its context intact rather than
being marked failed. A notification goes out. Nobody has to read a transcript to
find out the box is broken.

The residual risk is asymmetric on purpose. A false positive ends a run the
board leaves live and retryable; a false negative is a paralyzed row nobody can
see.

### Not in this issue

- **Detecting the paralysis generically.** This is the codex adapter reading
  codex's own command results. A harness-independent "this run has executed
  nothing in N minutes" watchdog is a different piece of work with a different
  false-positive profile (a long single command looks the same).
- **The escalation.** `WORKTREE_MOUNT_FIXED_IN` and
  `worktree_on_slashed_branch` are untouched. That gate is about a codex bug
  fixed in 0.147.0; this was our list, on a current CLI.
- **Granting `packed-refs` some other way.** A bind of the file itself, or the
  shared root plus a deny for `hooks/`/`config`, would both be new surface for a
  write no attempt makes. If a real workflow ever needs a packed ref deleted,
  that is the point to design it.
