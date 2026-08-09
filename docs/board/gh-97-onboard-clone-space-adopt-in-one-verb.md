# `onboard`: clone, space, adopt in one verb — **done** (gh#97)

Putting a repo on the board took three mechanisms that knew nothing about each
other: a clone somebody made on the box by hand, a `createSpace` from the
desktop (or, the night before this landed, a hand-built RPC seeder), and
`comet-board adopt`. The App side had already stopped needing a human —
all-repos installations — and #75 made routing remote. The clone and the space
were the last thing that still wanted a shell on the box.

`comet-board onboard <owner/repo> [--dir <path>] [--labels a,b | --all-issues]`,
and every step of it happens **on the device that hosts the board**:

1. **Resolve** against GitHub under the board's own credential. Not a
   formality — the laptop running the command usually has no GitHub credential
   at all, so resolving locally would answer about the wrong world; and a repo
   the App cannot see is one that would clone, get a space, get a route, and
   then poll nothing forever. A refusal names whoever can fix it, which under an
   App is the *installer* and not the operator: nothing on the box can grant an
   installation, so sending them to `.env` would be sending them nowhere.
2. **Clone**, with the askpass minting #68 built (`clone_env` →
   `git_credentials::agent_env`). The board's App is the credential that will
   push this repo's branches; it should be the one that fetched it. The URL that
   authenticates carries `x-access-token@`, so the remote is rewritten to the
   canonical one afterwards — a checkout outlives the clone that made it, and
   the human who opens that folder should find the remote they would have typed.
   `adopt::github_slug` learned to read the userinfo form anyway, because an
   interrupted onboard must not leave a checkout that detection cannot see.
3. **`createSpace`** — the same op the desktop picker sends, with
   `git_detected` stated rather than guessed (we just cloned it).
4. **Adopt** — `adopt_with` unchanged, through the same validating writer.
   Deliberately *not* via `WriteBoardConfig {op: adopt}`: that path detects
   through the spaces **watch**, which has not necessarily observed the row
   created three lines earlier, and an onboard that raced its own space would
   report "not on the unadopted list" about a repo it had just cloned. The
   polled/routed decision itself is shared — `adopt::missing_for`, factored out
   of `detect` — so the two surfaces cannot disagree about what "on the board"
   means.

**Idempotent at every step**, because the failure it exists to remove is a
*half*-onboarded repo: a clone with no space, a space with no route, a route
for a repo nothing polls. Re-running has to be the repair, not a second mess.
An existing checkout of the same repo is reused, an existing space for that path
is reused (`create_space` dedupes on `(device, path)` anyway, but silently —
reading the row back is what keeps the reply's `spaceId` honest), and a repo
already both polled and routed says so and writes nothing. What is *not* reused:
a directory holding something else. A checkout of a different repo is the
dangerous case — every step downstream would succeed, and the board would
dispatch this repo's issues into another repo's code — so it is refused.

- **`crates/board/src/onboard.rs`** holds the decisions and the report; the
  engine holds the effects, because the clone is `repos.rs`'s and the space is
  the workspace doc's. `Repos` gained `clone_to` (exact path, credential
  environment, canonical remote, and a failed clone cleaned up so a retry is a
  clean retry) and `origin_url`.
- **`OnboardRepo` / `ListAppRepos`**, both forwardable for the reason the
  config pair is: all of it belongs to the host. Two blocking phases inside the
  handler rather than one — the GitHub clients hold `Rc`s and cannot cross the
  await the clone needs — which costs one extra installation-token mint per
  onboard and is the right price for not holding a `!Send` value across a
  `git clone`.
- **`ListAppRepos` is the App's grant**, not the operator's repos: exactly the
  set the box can clone and the loop can poll, gathered across every
  installation under installation tokens (`/installation/repositories` is the
  one endpoint that answers *about* an installation and so names no repo to
  derive its credential from — hence `AppAuth::token_for_installation`). Repos
  already on the board stay in the list rather than being filtered out; "is this
  one already set up?" is half of why anybody opens the picker.
- **Settings → Board routing** grew the "Onboard a repo…" panel: the App's list
  with a button per row, plus a free-text field, which is not a fallback for a
  broken list — a board on `GITHUB_TOKEN` has no installations to enumerate at
  all, and the picker would otherwise be empty for it forever. A `--dir` field
  beside it, expanded against the *box's* home.
- **Writeback is reported, never set.** It is off by default on purpose —
  writing to somebody's issues is not a thing to start doing because a repo was
  pointed at the board — so onboarding says where it stands and leaves the
  decision where it was. Same for an archived repo and one with issues disabled:
  both would otherwise be discovered as a board that stays empty.
