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

### The recipe

`.comet/repo.toml`, in the repository, owned and reviewed like any other file:

```toml
version = 1

[setup]
run = "scripts/setup.sh"     # idempotent; sh -c, from the approved snapshot root
timeout = "10m"              # engine ceiling 1h; absent = 10m
outputs = ["target"]          # top-level untracked directories setup may write

[run]
run = "cargo run -p comet"   # the canonical dev command — recorded, never guessed

[archive]
paths = ["target"]            # engine-owned cleanup; no shell runs at reclamation

[env]
RUST_BACKTRACE = "1"         # values safe to commit — they are in the open

[[link]]
from = "dev.env"             # a leaf under {data_dir}/locals/{repository identity}/
to = ".env.local"            # a path inside the checkout
```

A directory (`.comet/`) rather than a bare file at the root because gh#273's
per-repository MCP configuration wants a home beside this one. That issue now
injects route-owned `mcp_servers` into the chat config. The recipe parses but
does not act on an `[mcp]` table: it is the repository-owned seam, while the
route remains the authority until MCP trust and merge rules are designed.

Everything is strict. Unknown keys are refused (`timout = "20m"` silently
meaning ten minutes is precisely the class of failure this issue is about),
another `version` is refused by name, a timeout past the 1-hour ceiling is
refused rather than clamped. The engine executes what it understood or
declines to execute at all.

### The lifecycle

`preparing → ready | failed`, persisted per checkout under
`{data_dir}/checkout-prep/{hash of the canonical path}/`:

- `prep.json` — the record: state, recipe and committed-tree digests, command, exit code, one
  human sentence, what every `[[link]]` did, and the recipe's `run` command so
  whoever holds the status also holds the offer.
- `prep.log` — the step's interleaved stdout+stderr, command at the top,
  capped at 256KB **keeping the head**: the first error diagnoses a broken
  setup, and the 200MB of `npm install` progress that pushed it out is a file
  nobody can read anyway.
- `brief.json` — the durable `parked | claimed | revoked` handoff; cancellation
  leaves a tombstone so a late setup completion cannot enqueue the agent.

The live attempt stores the same lifecycle in `board.db`, and `TaskRow` carries
it to the CLI, desktop, and phone: state, diagnostic/log path, approval need,
and every projected file. The first dispatch frame already says `preparing`;
it does not wait for the session poll, because there is deliberately no session
until preparation succeeds.

The recipe is read from the immutable `HEAD` object, and approval/idempotency
bind to the whole committed Git tree rather than the TOML alone. A checkout
that is `ready` for this execution digest is not prepared again (a retry lands
in an already-warm worktree at zero cost), but only while the live engine can
authenticate the matching approval grant. `prep.json` is evidence, not
authority: a forged Ready record is ignored, and restarting the engine expires
its process-private approval key and requires review again. Changing a
referenced script, lockfile, hook, or the recipe itself changes the digest;
tracked worktree/index edits are refused before host execution so they cannot
borrow the approval of `HEAD`. The CLI/UI returns the displayed execution
digest with approval as a compare-and-swap; a branch switch between review and
confirmation is refused. Only an authenticated `ready` short-circuits — a
`failed` record retries on the next visit, and a `preparing` record left by a
killed engine reads as failed and re-runs, which is why the contract demands
setup be idempotent.

Before setup, Comet materializes the exact approved Git tree into a private
execution snapshot. That source tree is read-only in the host filesystem
sandbox (Seatbelt on macOS, bubblewrap on Linux; unsupported hosts fail
closed), and it contains no `.git`, index, hooks, or mutable checkout path.
Each `[setup].outputs` entry must be a top-level path absent from the approved
tree. It is backed by a private writable directory and attached to the snapshot
at that name; every other repository path is read-only. Existing declared
output is seeded for idempotent retries, and successful output is promoted
back into the real checkout only after the command exits zero and the checkout
still matches the approved tree. Machine-local links are copied into the
snapshot for setup and projected into the checkout only at successful
settlement. Replacing `scripts/setup.sh` in the mutable checkout while setup is
running therefore changes neither the bytes executed nor their host reach.

Bounded, all of it: the host home, mutable checkout, Git metadata, and unrelated
paths are not in the setup process's filesystem view. It runs in its own
process group (`sh` alone dying while its `npm` grandchild runs on is how a
bounded setup becomes an unbounded one that also lies about having stopped),
SIGKILLed on timeout or cancellation; output is capped, and the recipe may
shorten the budget but never lift it past the ceiling.

### Where it sits in a dispatch

`CometRuntime::dispatch` used to queue the brief as soon as the chat existed.
Now:

1. worktree, account, conventions, chat — exactly as before;
2. **no recipe → the brief is queued inline, unchanged, at the same cost.**
   This is every repository that has not written one;
3. with a gating recipe: the brief is **parked durably** in the checkout's
   state directory before preparation can start. A parking failure rolls the
   new chat and worktree back instead of producing an unrecoverable attempt;
   the transcript says "preparing, nothing is billed yet", and preparation is
   spawned. `dispatch` returns; the board loop is not held behind a five-minute
   `npm install` (it is one thread — awaiting there would stall every other
   task's status, cancel and sync);
4. preparation succeeds → the parked brief is released into the command
   ledger and the run starts, exactly the send that step 2 would have made;
5. preparation fails → **nothing is queued.** The checkout is preserved, the
   failure and the log head are written into the chat, and the row shows a
   live attempt whose agent never started and never cost anything.

Recovery reuses everything: the board's ordinary Retry and `PrepareCheckout`
(below) re-run the recipe against the same worktree, and on success release
the same parked brief
through the same `settle_preparation` — the same attempt continues, no second
attempt is minted, no new checkout is cut. Delivery is one durable
`parked → claimed → removed` state machine plus a command id minted before
parking. The command-ledger snapshot is durably saved before the handoff is
removed, and engine startup reconciles any `ready` checkout still carrying a
parked or crash-abandoned claimed handoff. A cancel uses the same lock and
writes `revoked` through the attempt's own worktree path, not mutable chat cwd;
the attempt stays live if that tombstone cannot be made durable. A late
completion therefore cannot enqueue behind it even when the chat was deleted
or retargeted. Two racing retries cannot both release the brief. `Run` is kept
Pending until the sessions engine owns its live handle, then marked processed;
a crash before spawn retries it after restart, while the in-process start claim
prevents a duplicate. Startup installs GitHub push credentials before releasing
ready handoffs, so a recovered GitHub run is not durably rejected during boot.

The declarative `[archive].paths` list rides reclamation:
`reclaim_build_output` removes those approved top-level reproducible
directories first, then gh#186's name-list sweep as before — the repo knows
about generated output the generic list could not have guessed, and the list
catches the repo that never wrote a recipe. The approved path list is retained
in live engine-authenticated state, so ordinary agent commits cannot replace
cleanup reach; an engine restart expires that grant and skips
repository-authored cleanup until the operator reviews it again. Archive never
evaluates a repository command or mutable script after the agent has worked.
Paths are relative, top-level, capped at 16, and may not name `.git` or
`.comet`. Deleting a worktree forgets its preparation record; paths get reused,
and a stale `ready` on a re-cut path would short-circuit a checkout that was
never prepared.

### Ordinary sessions — the board composes, it does not own

The primitive is `comet_engine::checkout_prep::CheckoutPrep`, on the engine
core, held (not wrapped) by the board runtime. RPCs expose it to every
frontend. Preparation and cancellation route to the device that owns the
checkout; approval deliberately does not, because trust is granted only by an
operator already on that host:

- `CheckoutPrep { worktreePath }` — read-only: the record, the parsed recipe,
  and a parse error said out loud (a viewport offering nothing because the
  file is malformed and one offering nothing because there is no file are not
  the same thing to the person looking at it).
- `PrepareCheckout { worktreePath, repoPath?, force? }` — run the recipe. The
  repository identity is always derived from `worktreePath`; an optional
  `repoPath` is compatibility metadata and is refused when it identifies a
  different repository, never accepted as authority.
  `force` defaults to **true** here (a person pressed this because the last
  answer was wrong; handing back the cached `ready` helps nobody) and false on
  the automatic path.
- `CancelCheckoutPreparation { worktreePath }` — cancel the checkout-owned
  token and kill the command's process group.
- `ApproveCheckoutPreparation { worktreePath, expectedExecutionDigest }` —
  embedded-operator only: approve the exact stable repository identity plus
  the committed-tree execution digest the operator just reviewed. A changed
  digest is refused. Localhost IPC is deliberately not operator authority; an
  agent or setup process calling the public port is refused.

The embedded desktop is the approval surface. It shows the setup command,
declared writable outputs, archive paths, every machine-local source and
destination, and the execution digest, then uses a private in-process operator
transport. The engine signs the retained grant with a process-private HMAC key
and verifies it before either execution or a Ready cache hit; same-user writes
to the trust directory cannot mint authority. `comet-board
approve-preparation` intentionally refuses even from an interactive PTY: a
TTY is not user presence and a dispatched DangerFullAccess agent can create
one. Plain `comet-board retry` does the in-place retry when no new approval is
needed.

Deliberately a verb, not something `CreateWorktree` does on its own:
preparation runs the repository's code, and a checkout appearing in a sidebar
is not the moment to start doing that behind somebody. A dispatch prepares
automatically because it is about to hand the same checkout to an agent that
would run that code anyway.

### Secrets and the shared box

The reference implementation this was researched against discovers `.env`
files by walking the machine. That is the one behavior deliberately not
ported. Comet runs shared boxes and dispatches work for teammates, and
`.comet/repo.toml` is writable by anyone who can open a pull request. There are
two independent boundaries: paths are constrained by construction, and every
repository-authored effect requires an engine-owned trust decision:

- **A recipe cannot approve itself.** `[[link]]` projection, `[setup]`
  execution, and `[archive]` cleanup occur only after the host approves the
  exact repository identity + committed-tree digest. Any committed edit
  invalidates approval; any tracked uncommitted edit is refused. Before that, the
  lifecycle is `failed` with `requires_approval = true`: no file is projected
  and no command is spawned.
- **Localhost and a PTY are not a person.** Public IPC, relayed callers, and the
  host CLI never carry the operator bit. Only the embedded desktop's private
  in-process client can call the approval RPC. Persisted grants are
  process-signed, so writing the store directly is not an approval.
- **Approved code still gets a narrow host view.** Environment clearing is not
  filesystem isolation. Seatbelt/bubblewrap expose toolchain/system roots and
  the immutable approved snapshot read-only, plus only private declared output
  roots writable. The mutable checkout, Git metadata, `$HOME`, and arbitrary
  host credentials outside explicit links are absent. Missing sandbox support
  is a preparation failure, never an unsandboxed fallback.
- **The child does not inherit the engine's environment.** It receives a small
  shell baseline (`PATH`, user/shell names and locale), an empty engine-owned
  `HOME` and temporary directory, the committed `[env]`, and `COMET_WORKTREE`
  (the immutable execution root)/`COMET_PREPARE`. Board credentials, GitHub
  tokens, askpass state, SSH/npm/Cargo credentials, tool-manager homes and
  unrelated ambient values do not ride along. Anything intentionally
  machine-local uses an explicit `[[link]]` instead.
- **A `[[link]]` names a leaf; comet names the root.** `from` resolves under
  `{data_dir}/locals/{hash of stable repository identity}/`, a directory the
  operator fills by hand. The identity hashes the device and Git common dir,
  so unrelated repositories with the same basename never share a namespace.
  Absolute paths and `..` are refused at parse, on the spelling, before
  anything exists to check. `~/.ssh/id_ed25519` is not reachable because it is
  not *spellable*.
- **Symlinks at the leaf or in any source/destination ancestor are refused,
  not followed.** Unix traversal and creation are descriptor-relative
  (`openat`/`mkdirat`, `O_NOFOLLOW`, create-exclusive), so swapping an ancestor
  after it was checked cannot redirect the eventual read or write.
- **Projection publishes atomically, never via symlink, preserving mode.** Unix
  copies into a private create-exclusive sibling, preserves mode, fsyncs it,
  publishes with no-replace `linkat`, fsyncs the directory, then removes the
  temporary name. A crash cannot leave a partial credential at the requested
  path. Hosts without descriptor-relative/no-follow semantics disable links
  rather than using a weaker fallback.
- **Ownership stays explicit** — Unix sources must belong to the engine uid;
  the copied destination belongs to that same uid. A shared host never silently
  changes ownership on another user's credential.
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

### Exit criteria, answered

- *Visible `preparing → ready | failed` lifecycle* — `PrepState` persisted per
  checkout and on the attempt, served by `CheckoutPrep`, streamed on `TaskRow`,
  summarized into the chat transcript, and rendered by both board viewports.
- *Agent not started or billed until preparation succeeds* — the brief is
  parked before preparation begins and released only by `settle_preparation`
  on a `ready` record; a failure queues nothing.
- *Idempotent, bounded, cancellable, diagnosable* — committed-tree digest
  short-circuit + re-run-on-failure; immutable snapshot with explicit writable
  outputs; host filesystem sandbox; process-group kill on a ceilinged timeout
  and `CancellationToken`;
  head-kept capped log that outlives the attempt.
- *Retry reuses the worktree* — Board Retry and `PrepareCheckout` run against
  the existing checkout and release the existing parked brief; the attempt
  row, chat and branch all continue.
- *Works for ordinary sessions; the board composes engine primitives* — one
  `CheckoutPrep` on the engine core, shared by the board runtime and the RPC
  surface; the board has no private setup path.
- *Relationship to #273 explicit* — route `mcp_servers` remains the current
  chat authority; the recipe's `[mcp]` table parses as the repository seam and
  acts never; `.comet/` is the shared home.

### License boundary

Zuse (AGPL-3.0-only) was product research for this design: which verbs a
recipe needs, what the lifecycle owes the operator, what file-linking gets
wrong on shared machines. No source, schema, test or text was taken; the
format (TOML vs its TypeScript contract), the rooted-allowlist link model (vs
its discovery), the parked-brief recovery and the digest short-circuit are
comet's own, written against comet's architecture. If closer reuse is ever
wanted, the license question comes first.

### Not in this issue

- **Repository-owned MCP authority.** gh#273's route-owned injection exists;
  merging it with the recipe's `[mcp]` seam is a separate trust decision.
- **Zuse-style worktree defaults** (per-repo worktree root/naming) — routing
  already owns that on the board path.
- **A recipe for the operator's own space folder.** Preparation is about
  fresh checkouts; the folder somebody works in by hand is already theirs.
