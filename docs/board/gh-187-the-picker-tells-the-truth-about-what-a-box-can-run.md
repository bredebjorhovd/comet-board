# The picker tells the truth about what a box can run — **done** (gh#187)

Half of "every harness reachable in every project" already worked: a dispatch
can name any runtime (`--runtime codex`, the desktop and phone pickers), it is
validated against the engine's catalog, and an unknown name is refused rather
than guessed at. The other half was a lie the pickers told.

`runtime::runtime_options()` was a **static list** — Claude Code, OpenCode,
Codex, Cursor, Mock — with no reference to the device the work would land on.
Measured on the box: `claude` and `codex` installed, `opencode` and
`cursor-agent` not. So the picker offered OpenCode, the dispatch was accepted,
a worktree was cut, a chat was created — and only then did the harness spawn
fail. **The board spent the expensive part before checking the cheap fact.**

- **Availability is a fact about a device, not a constant.**
  `ListBoardRuntimes` was already relay-forwardable; it now answers for the
  *target* device. Two probes, both cheap, both offline:
  `comet_harness::locate_cli` (the same resolution the adapter does at spawn,
  minus the spawn) and `AgentAccounts::signed_in` (the same files the accounts
  page reads, minus the network). `comet_engine::runtimes` is the one place
  they are combined, so the picker, the dispatch guard and `doctor` cannot
  disagree.
- **Installed and signed-in are named apart**, because they are two different
  jobs for whoever reads them. Codex sat installed-but-signed-out on the box
  for twenty minutes, and a dispatch in that window looked identical from the
  picker to one that would have worked. `RuntimeUnavailable` carries
  `notInstalled` / `signedOut` / `unsupported` — the last for `cursor`, which
  is in the runtime table and has never had an adapter, so "not installed"
  would send an operator off to install something that would not help.
- **Refused at dispatch, beside the cap and the billing guard** (§gh#101's
  position, for §gh#101's reason): before the attempt row, so a refusal costs no
  cleanup. `board_runtime.rs` had already established the discipline for the
  account — resolved before the chat exists, because "an attempt whose chat
  exists but whose login does not is a row somebody has to clean up" — and an
  unavailable runtime gets the same treatment. The `comet-board` CLI refuses it
  one round trip earlier, in the same words.
- **A named account answers for itself.** A run pointed at a slot reads that
  slot's materialized config dir and never the CLI's own, so the box's login
  being absent says nothing about it — the signed-out half is skipped, and the
  slot is checked for real when the executor materializes it.
- **Shown, never filtered.** An unavailable runtime stays in the picker,
  dimmed, saying why: an operator who expects OpenCode on a box needs to read
  "not installed", not find the row absent and wonder which box it was. The
  route's own runtime is still where the cursor starts even when the host
  cannot run it — that sentence is the point — while the *fallback* prefers an
  available option, so the picker never invents a dead end nobody chose.
- **`doctor` says it from the shell.** A `harnesses` line naming what is ready
  and why the rest is not. It fails only when *nothing* can start, which is a
  board that can poll and derive and never dispatch; one missing runtime out of
  four is a choice. `mock` is left out of the census — always available, so
  counting it would let an empty box report "1 ready" and pass.
- **Routes keep naming a default runtime.** Most work has an obvious harness,
  and the per-dispatch override stays the way to reach for a different one.
  What changed is that both now fail loudly and early when the box cannot
  honour them.
