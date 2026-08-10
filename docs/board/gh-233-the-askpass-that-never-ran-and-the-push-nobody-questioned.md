# The askpass that never ran, and the push nobody questioned — **done** (gh#233)

The first opencode dispatch (gh#222, PR #231) reported that its `git push`
could not exec the board's askpass helper, and got the push through by writing
a credential wrapper of its own. Two separate failures, and the second is the
one worth the ticket.

### The askpass was wrong, and had been all along

§gh#68's module says, in a comment the quoting depends on:

> `GIT_ASKPASS` names one command, and git runs anything with a space in it
> through `sh -c '<cmd> "$@"'`.

That is `credential.helper`'s rule. `GIT_ASKPASS` names **one executable**,
which git execs directly with the prompt as its single argument — no shell, no
argument splitting, nothing for a quote to protect. So
`'<path>/comet-board' git-askpass` was taken as a filename, no such file
existed, and git said `cannot exec` and gave up.

- **It was never opencode.** The reproduction is four lines of shell and no
  harness at all: a `GIT_ASKPASS` with an argument in it fails on Apple git
  2.50.1 and on the box's 2.53.0 alike, while a bare path — *including one with
  a space in it* — works. The quoting that existed to survive
  `/Applications/Some App/…` was the thing breaking it. What was different about
  the opencode run was not its child process; it was that it was the first
  dispatch on a box that had both a board credential and a resolvable
  `comet-board`, and so the first one to reach this code at all.
- **The unit test passed throughout**, because what it asserted was the shape of
  a string. `crates/board/tests/askpass_git.rs` asserts against `git`: a
  listener answers `401` the way GitHub does, and the test reads the
  `Authorization` header git then sends. That header can only be right if git
  could exec what it was handed, if the subcommand survived, and if the answer
  came back on the pipe. The old form is kept as a second test, and fails the
  way it failed in production.
- **The subcommand moved into a file.** `install_askpass_shim` writes
  `comet-askpass` beside the `gh` shim — `exec '<board>' git-askpass "$@"` — and
  `GIT_ASKPASS` gets its path and nothing else. A path is all the variable can
  carry, so a path is all it is given.
- **The path is now run before it is handed over.** `verify_askpass` asks the
  shim git's username prompt, which needs no credential, no network and no
  board, and requires `x-access-token` back. It exercises every layer except the
  mint for the price of one `fork`, at dispatch and in `doctor`. A device that
  was configured to issue the board's credential and cannot is a **fault** now,
  logged at error, and no longer indistinguishable from a device that was never
  asked to.

### The push that went around it is the durable half

The run *succeeded*. §gh#68 put real care into the token never landing in argv,
in `.git/config`, or in the environment, because several people drive that box;
a wrapper written under time pressure by an agent whose push just failed has
none of those properties reviewed. Nothing recorded that it happened. The only
reason anybody knows is that the smoke ticket asked the agent to mention
anything odd.

- **The credential path keeps a ledger** (`credential_ledger`): one line of JSON
  per event under the board's state dir. `handed` when the engine wires a run to
  the helper, `minted` when the helper answers, `failed` when it runs and
  cannot, `unusable` when the path itself does not work. Attributed by
  `COMET_BOARD_CHAT_ID`, which every process a dispatched agent starts inherits
  — so a mint is attributable to an attempt without anybody threading ids. No
  secret is in it; a line names a repo, a chat and a tool.
- **The settle asks the question.** Every settle is a settle on work that
  reached origin (gh#69 made sure of that). If the board handed that chat its
  credential and the helper was never asked — or could not be handed over at all
  — then something else pushed, and the board says so: an error in the log, a
  clause on the settle notice the dispatching agent hears, and a comment on the
  issue. It does not accuse anyone of anything; it says the board cannot account
  for the credential that pushed, and asks for the branch to be looked at.
- **What stays quiet is as important.** A run whose helper minted is silent, and
  so is a box with no board credential at all — it never claimed to be the thing
  that pushes, and every device pushed that way before §gh#68.
- **`doctor` runs the path** rather than counting its parts. The old check —
  a credential exists, a binary resolves — is exactly the check gh#233 passed.
  It also surfaces the last recorded failure, because a failure that happened
  inside somebody's run beats one synthesised for a diagnostic.
- **The conventions say it outright.** `docs/agent-conventions.md` and the
  shipped skill now tell a dispatched agent that a push which cannot
  authenticate is a stop, not a puzzle to route around: say so, and do not write
  a credential path the board did not sanction. That is a request, and requests
  are not guarantees — which is why the ledger is the part that does not depend
  on anybody reading it.
