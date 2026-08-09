# Onboarding a teammate is a verb and a page — **done** (gh#162)

Every mechanism for a second person on the box had shipped: org visibility
(gh#66), invitations (gh#76), per-run agent accounts (gh#59), per-dispatch
authorship (§gh#107). What an operator must actually *do* existed only as three
unrelated lines of `doctor` output, each discovered after the fact — the first
one usually when a commit landed under the wrong name.

- **`comet-board member add <email> --github <login|address>`.** §gh#107 gave
  `git_identity.rs` a *reader* for `[users]` and no writer, so the map was a
  hand-edit on a box most teammates have no shell on. This is the writer, and
  it lands as `routes::Edit::User` — the same parse + validate + `.bak`
  discipline as every other config edit, and the same forwarded
  `WriteBoardConfig`, so `--device` reaches the box. Both halves are refused by
  name rather than written: a value that is not an address would land on
  `GIT_AUTHOR_EMAIL` and produce exactly the unattributable commits §gh#107
  exists to prevent, and a key that is not a sign-in email writes an entry no
  dispatch can ever match. Idempotent in the way that matters — a bare login
  resolves to the address GitHub minted for it, so `--github ana` and the same
  person's pasted noreply address produce a byte-identical line, and a second
  spelling of one email corrects the entry it already has rather than adding a
  case-variant twin the reader would resolve arbitrarily.
- **The resolution happens on the box, for §gh#97's reason.** A bare login needs
  `GET /users/{login}` to learn the numeric id, and the credential that can ask
  is the *board's* App — the laptop running the command usually has none, which
  is the whole reason the map was an ssh-and-edit job. An address needs no
  round trip and takes none.
- **`member list` is the pairing nobody thinks about.** A mapped teammate with
  no agent-account slot of their own commits as themselves and spends somebody
  else's subscription. The two facts live in different places — `routing.toml`
  and the engine's saved CLI logins — which is exactly why nothing had put them
  on one line. The comparison is the billing guard's own (slot email against
  the sign-in identity, case-insensitively): two rules for one question is how
  a surface ends up confidently wrong. `doctor`'s `dispatch authorship` line
  gained the same sentence, so the box says it without being asked. "No slots"
  and "the engine could not be asked" stay different answers (§gh#155).
- **`docs/teammate.md`, named by the failure.** Invite → they install and paste
  the code → `member add` → their account slot → the App on their repos, in
  that order, each with what it fixes if skipped and which `doctor` line
  confirms it. Linked from the README beside the orchestrator brief, and named
  by the `doctor` line and the CLI's own follow-up, so the failure points at
  the fix rather than at a section number.
- **A test that was reading the live board.** `Paths::under` honours
  `COMET_BOARD_CONFIG_DIR` / `COMET_BOARD_STATE_DIR` — right for the engine,
  since an operator pointing the board elsewhere means it, and wrong for a
  test, because those variables are set in the environment of every
  board-dispatched agent. `device_routing.rs` took its tempdir, had it ignored,
  and ran against the box's own board: the forwarded-rows assertion failed on
  the *real* queue, and every config write in it landed in the operator's
  hand-edited `routing.toml`. Both engine tests now build `Paths` by hand. The
  symptom had been read as flakiness; it was a test suite writing production
  config. (Building them by hand was a *convention*, and it held for exactly the
  two files that were looked at — see §gh#190.)
- **Not in scope, and said out loud in both places.** Enforcing *whose*
  subscription a run spends is gh#161; this is the map and the instructions
  being writable and discoverable. `member remove` is here because offboarding
  with no verb is the same hand-edit from the other side, and it does only its
  own step — the slot, the org membership and the chats are three different
  revocations, and one verb doing all three would make the reversible one look
  as final as the other two.
