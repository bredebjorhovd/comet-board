# A checkout is not ready until the repository setup recipe says it is (gh#422)

A board dispatch cuts an isolated worktree in milliseconds and starts an agent
in it immediately. Git isolation was solved a long time ago; environment
readiness never was. The checkout has no `node_modules`, no generated code, no
machine-local `.env.local`, and no written-down answer to "what runs this" —
so every agent rediscovered setup in prose, and every rediscovery was billed,
because the discovering happened *after* the model started. The failures
arrived the same way: twenty minutes into paid time, as a sentence in a
transcript.

This gives a repository a reviewed, repeatable way to say what a fresh
checkout needs, and gives the engine a rule: **the agent does not start, and
is not billed, until the recipe says the checkout is ready.**

## The recipe

`.comet/repo.toml`, in the repository, owned and reviewed like any other file:

```toml
version = 1

[setup]
run = "scripts/setup.sh"     # idempotent; sh -c, from the worktree root
timeout = "10m"              # engine ceiling 1h; absent = 10m

[run]
run = "cargo run -p comet"   # the canonical dev command — recorded, never guessed

[archive]
run = "cargo clean"          # bounded cleanup before reclamation

[env]
RUST_BACKTRACE = "1"         # values safe to commit — they are in the open

[[link]]
from = "dev.env"             # a leaf under {data_dir}/locals/{repoName}/
to = ".env.local"            # a path inside the checkout
```

A directory (`.comet/`) rather than a bare file at the root because gh#273's
per-repository MCP configuration wants a home beside this one; the recipe
already parses (and ignores) an `[mcp]` table so a repository that fills it in
early is not a parse error on today's engine. That table is the explicit seam:
what an MCP server is allowed to be on a shared box is gh#273's question, and
nothing here answers it.

Everything is strict. Unknown keys are refused (`timout = "20m"` silently
meaning ten minutes is precisely the class of failure this issue is about),
another `version` is refused by name, a timeout past the 1-hour ceiling is
refused rather than clamped. The engine executes what it understood or
declines to execute at all.

## The lifecycle

`preparing → ready | failed`, persisted per checkout under
`{data_dir}/checkout-prep/{hash of the canonical path}/`:

- `prep.json` — the record: state, recipe digest, command, exit code, one
  human sentence, what every `[[link]]` did, and the recipe's `run` command so
  whoever holds the status also holds the offer.
- `prep.log` — the step's interleaved stdout+stderr, command at the top,
  capped at 256KB **keeping the head**: the first error diagnoses a broken
  setup, and the 200MB of `npm install` progress that pushed it out is a file
  nobody can read anyway.
- `brief.json` — see recovery.

The record is keyed by the checkout and by the sha256 of the recipe: a
checkout that is `ready` for this digest is not prepared again (a retry lands
in an already-warm worktree at zero cost), and an *edited* recipe re-prepares
without anybody remembering to force anything. Only `ready` short-circuits — a
`failed` record retries on the next visit, and a `preparing` record left by a
killed engine reads as failed and re-runs, which is why the contract demands
setup be idempotent.

Bounded, all of it: the step runs in its own process group (`sh` alone dying
while its `npm` grandchild runs on is how a bounded setup becomes an unbounded
one that also lies about having stopped), SIGKILLed on timeout or
cancellation, output capped, and the recipe may shorten the budget but never
lift it past the ceiling.

## Where it sits in a dispatch

`CometRuntime::dispatch` used to queue the brief as soon as the chat existed.
Now:

1. worktree, account, conventions, chat — exactly as before;
2. **no recipe → the brief is queued inline, unchanged, at the same cost.**
   This is every repository that has not written one;
3. with a recipe: the brief is **parked** in the checkout's state directory,
   the transcript says "preparing, nothing is billed yet", and preparation is
   spawned. `dispatch` returns; the board loop is not held behind a five-minute
   `npm install` (it is one thread — awaiting there would stall every other
   task's status, cancel and sync);
4. preparation succeeds → the parked brief is released into the command
   ledger and the run starts, exactly the send that step 2 would have made;
5. preparation fails → **nothing is queued.** The checkout is preserved, the
   failure and the log head are written into the chat, and the row shows a
   live attempt whose agent never started and never cost anything.

Recovery reuses everything: `PrepareCheckout` (below) re-runs the recipe
against the same worktree, and on success releases the same parked brief
through the same `settle_preparation` — the same attempt continues, no second
attempt is minted, no new checkout is cut. The park is a file, so it survives
an engine restart; the take is a remove, so two racing retries cannot both
release one brief.

The `[archive]` step rides reclamation: `reclaim_build_output` runs the
repository's own cleanup first (best-effort, its own shorter budget), then
gh#186's name-list sweep as before — the repo knows about the generated
directory the list could not have guessed, the list catches the repo that
never wrote a recipe. Deleting a worktree forgets its preparation record;
paths get reused, and a stale `ready` on a re-cut path would short-circuit a
checkout that was never prepared.

## Ordinary sessions — the board composes, it does not own

The primitive is `comet_engine::checkout_prep::CheckoutPrep`, on the engine
core, held (not wrapped) by the board runtime. Two RPCs expose it to every
frontend, relay-forwardable like the rest of the checkout verbs because a
recipe executes where the working tree is, never where the button was pressed:

- `CheckoutPrep { worktreePath }` — read-only: the record, the parsed recipe,
  and a parse error said out loud (a viewport offering nothing because the
  file is malformed and one offering nothing because there is no file are not
  the same thing to the person looking at it).
- `PrepareCheckout { worktreePath, repoPath?, force? }` — run the recipe.
  `force` defaults to **true** here (a person pressed this because the last
  answer was wrong; handing back the cached `ready` helps nobody) and false on
  the automatic path.

Deliberately a verb, not something `CreateWorktree` does on its own:
preparation runs the repository's code, and a checkout appearing in a sidebar
is not the moment to start doing that behind somebody. A dispatch prepares
automatically because it is about to hand the same checkout to an agent that
would run that code anyway.

## Secrets and the shared box

The reference implementation this was researched against discovers `.env`
files by walking the machine. That is the one behavior deliberately not
ported. Comet runs shared boxes and dispatches work for teammates, and
`.comet/repo.toml` is writable by anyone who can open a pull request — so the
recipe's reach is constrained by *construction*, not by filtering:

- **A `[[link]]` names a leaf; comet names the root.** `from` resolves under
  `{data_dir}/locals/{repoName}/`, a directory the operator fills by hand.
  Absolute paths and `..` are refused at parse, on the spelling, before
  anything exists to check. `~/.ssh/id_ed25519` is not reachable because it is
  not *spellable*.
- **A symlink inside the locals directory is refused, not followed** — one
  `ln -s` would otherwise reintroduce exactly the reach the root removed.
- **Projection copies, never symlinks, preserving mode** — a symlink out of
  the checkout is unreadable to a sandboxed run and editable-through by an
  unsandboxed one; a 0600 credential that lands 0644 is a finding.
- **Never clobbers** — an existing destination is kept and said so; a missing
  source is recorded (`missing`) rather than failing the dispatch, because an
  operator convenience must not become a hard dependency of every attempt.
- **What every link did — source, destination, result, mode — is on the
  record before the agent starts.** Credential reach is a thing you read, not
  infer.
- **`[env]` cannot name `PATH`, the loader variables, `GIT_ASKPASS`, or
  anything `COMET_*`/`DYLD_*`** — a file in a pull request must not be able to
  point a teammate's agent at another board (`COMET_BOARD_STATE_DIR`), inject
  into every process the setup starts (`LD_PRELOAD`), or silently re-point
  every binary name in the script a reviewer just read (`PATH`).

## Exit criteria, answered

- *Visible `preparing → ready | failed` lifecycle* — `PrepState` persisted per
  checkout, served by `CheckoutPrep`, summarized into the chat transcript.
- *Agent not started or billed until preparation succeeds* — the brief is
  parked before preparation begins and released only by `settle_preparation`
  on a `ready` record; a failure queues nothing.
- *Idempotent, bounded, cancellable, diagnosable* — digest short-circuit +
  re-run-on-failure; process-group kill on a ceilinged timeout and on a
  `CancellationToken`; head-kept capped log that outlives the attempt.
- *Retry reuses the worktree* — `PrepareCheckout` runs against the existing
  checkout and releases the existing parked brief; the attempt row, chat and
  branch all continue.
- *Works for ordinary sessions; the board composes engine primitives* — one
  `CheckoutPrep` on the engine core, shared by the board runtime and the RPC
  surface; the board has no private setup path.
- *Relationship to #273 explicit* — the `[mcp]` table parses today, acts
  never; `.comet/` is the shared home.

## License boundary

Zuse (AGPL-3.0-only) was product research for this design: which verbs a
recipe needs, what the lifecycle owes the operator, what file-linking gets
wrong on shared machines. No source, schema, test or text was taken; the
format (TOML vs its TypeScript contract), the rooted-allowlist link model (vs
its discovery), the parked-brief recovery and the digest short-circuit are
comet's own, written against comet's architecture. If closer reuse is ever
wanted, the license question comes first.

## Not in this issue

- **gh#273** — per-repository MCP configuration. The seam exists; the policy
  does not.
- **Blocking the run row on readiness surfaces.** The transcript and the prep
  record say the truth; a dedicated frontend affordance (a "preparing" pill on
  the board row, a retry button on the chat) is frontend work that reads the
  same record.
- **Zuse-style worktree defaults** (per-repo worktree root/naming) — routing
  already owns that on the board path.
- **A recipe for the operator's own space folder.** Preparation is about
  fresh checkouts; the folder somebody works in by hand is already theirs.
