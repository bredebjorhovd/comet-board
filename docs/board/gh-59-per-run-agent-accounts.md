# Per-run agent accounts — **done** (gh#59)

Whose Claude/Codex subscription a dispatch spends is a per-run choice, not an
engine-wide mode. Each teammate attaches their own login under Agent accounts;
a route's `account` (or `DispatchTask {account}` / `comet-board dispatch
--account`) names the slot, and that dispatch burns its owner's limits.

The mechanism is env, not files. `crates/engine/src/agent_accounts.rs`
materializes a slot into a config dir of its own — `{data_dir}/accounts/{slotId}/`,
holding `.credentials.json` + `.claude.json` for Claude and `auth.json` for
Codex — and the run stamps `CLAUDE_CONFIG_DIR` / `CODEX_HOME` at it in the
harness child's env, exactly as `RunControls::chat_id` becomes
`COMET_BOARD_CHAT_ID`. The alternative it replaces (`activate`, which overwrites
`~/.claude/.credentials.json`) is engine-wide and mutates what a *live* run is
reading — a footgun even for one user. `activate` remains, for choosing the
device's own CLI login; a run naming an account never touches it.

The dir is the live copy from then on: refresh writebacks the CLI makes land
there, `read_slots` absorbs them back into the slot file, and usage probes read
the result. A run holds a lease on its slot for its lifetime, which keeps the
usage refresher from rotating a refresh token the CLI is still holding — the
same rule that already applied to the active login.

The account rides `ChatConfig`, not `RunRequest`: a login belongs to the agent,
so every later turn in the chat (steers, review deliveries, an operator typing
into the same session) keeps spending it, and a steer arriving mid-turn cannot
change it. An account that will not resolve **refuses** the dispatch before the
chat exists, and refuses a later run rather than falling back — a silent
fallback bills whoever the device's own login belongs to.

One consequence the composer's `/` picker (gh#134) now makes visible: a slot
IS the run's `CLAUDE_CONFIG_DIR`, so what a dispatched agent can invoke is what
`{data_dir}/accounts/{slotId}/skills/` holds — the board's own skill that
`materialize` stamps there (gh#133) and nothing else — plus whatever its
checkout ships in `.claude/`. The user-level `~/.claude/skills` the operator
sees in their own sessions is invisible from inside a slot.

The picker reports that rather than offering the box user's list: offering a
list the run cannot invoke is worse than a short one. So a skill an agent is
*meant* to have belongs in the repo, or is installed the way the board's own is
— written into every slot on every dispatch, byte-compared, never fatal.

Deliberately not in v1, and still not: inferring an account from the WorkOS user
who dispatched. §gh#73 now records who released the work by name as well as by
chat, and that changes nothing here — guessing a login from either is the kind
of clever that bills the wrong person, and the identity it would guess from is
unverified. `comet-board doctor`
checks each route's `account` against the device's saved logins, including the
CLI it belongs to — a Claude slot on a codex route is not lendable, since the
two config-dir variables are not interchangeable.

All four are relay-forwardable (§gh#55): `targetDeviceId` = the box, and a
teammate's laptop reads and drives the box's board without hosting one.
