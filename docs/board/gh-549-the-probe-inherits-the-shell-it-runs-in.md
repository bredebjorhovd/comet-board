# The probe inherits the shell it runs in — **done** (gh#549)

`doctor` printed this, in red, from inside a comet chat:

```
FAIL dispatched pushes    the credential path does not work — no dispatched agent
                          on this box can push with the board's App: the askpass
                          helper exited status 1: "a board-dispatched credential
                          request has no persisted push contract"
```

Same box, same minute, one variable:

```
$ env -u COMET_BOARD_CHAT_ID comet-board doctor | grep 'dispatched pushes'
ok   dispatched pushes    the askpass helper answers, and mints per push; …
```

That is the whole bug. The credential path was fine; the probe was contaminated
by the environment of whoever ran `doctor`.

### What was wrong

Two lines, in two crates, working together.

`comet-board git-askpass` guarded every invocation:

```rust
let chat = std::env::var(ops::CHAT_ID_ENV).ok();          // COMET_BOARD_CHAT_ID
let contract = push_contract_from_env()?;                 // COMET_BOARD_PUSH_CONTRACT
if chat.is_some() && contract.is_none() {
    bail!("a board-dispatched credential request has no persisted push contract");
}
```

and the probe — `verify_askpass`, which `doctor`'s dispatched-pushes check runs —
built its child with no environment at all:

```rust
exec_waiting_out_busy(std::process::Command::new(askpass).arg(prompt))
```

So the shim inherited the operator's whole shell. Run `doctor` from any comet
chat and the child sees `COMET_BOARD_CHAT_ID` (every chat's shell carries it)
with no contract (nothing outside a dispatch sets one), and the guard fires
before the probe reaches the question it came to ask. The sentence is maximal —
*no dispatched agent on this box can push with the board's App* — and this is
the one check a person reads to decide whether the box works at all. It sends
you looking for a `gh` that fell off the PATH, an expired App key, a broken
install.

Agents are the main caller: every dispatched agent and every orchestrator runs
`doctor` from inside a chat, which is precisely the condition that produces the
false FAIL. A plain shell — the one case that passed — is the rarer one on the
box. And the `bail!` sat *before* `askpass_with_contract`, so it never reached
`credential_ledger::failed`: the ledger stayed clean while the line went red,
probe and ledger disagreeing, with #515's fix underneath dutifully dating a
failure that should never have been raised.

### Two fixes

Either one suffices; both landed.

- **The probe constructs its environment** (`git_credentials.rs`). `run_askpass`
  now starts from an empty environment and stamps the pair a dispatch stamps to
  say which board the helper attaches to (`COMET_BOARD_CONFIG_DIR` /
  `COMET_BOARD_STATE_DIR`) — so the answer is about the device whose files
  `doctor` read, whatever process raised it, and a `--data-dir` run checks the
  board it named. Nothing else is stamped on purpose: no repo, because the
  username question names none and an invented one could one day be minted
  against; no contract or chat id, because the secret-bearing half of a
  dispatch has its own probe in `verify_push_credential`, which already built
  the full `agent_env_with_contract`.
- **The guard is classified by prompt kind first** (`main.rs`, via the new
  `ensure_contract_for_prompt`). A username prompt is answered off the constant
  `x-access-token`; it emits no credential, so a missing contract cannot
  endanger anything by being absent for it. The demand now belongs to prompts
  that would mint — which is where it always belonged, next to the rule that a
  username answer is never recorded as a mint.

### Local vs device

`doctor` always reads *this device's* own files — `--device` explicitly does not
apply to it — but the check's sentences said "this box" and "no dispatched
agent", which on a laptop pointed at the box would be about the wrong machine.
They now say *device*, the word the rest of the engine uses for the machine a
check is standing on.

Sibling: #544, same week, same shape — a check reporting confidently about
something other than what it measured. And #515 before it: that fix taught the
*verdict* to tell history from the present tense; this one removes a failure
the present tense never contained.
