# Never spend someone's subscription silently — **done** (gh#101)

gh#59 made *which* account a dispatch spends explicit and gh#74 made every
frontend send one, and between them they left the case they had just made
visible unaddressed: a dispatch that names no account runs on the box's own CLI
login. On a shared box that is the owner's subscription, whoever pressed enter.
The teammate did not know they were spending it; the owner found out on their
usage page.

**Who a run bills** is resolved at dispatch: the named slot's
`AgentAccount.email`, or — when no slot is named — the box's own login, which is
the *active* account for that harness and is displayed as the operator's. It is
recorded on the attempt (`attempts.billed_to`) and joins the `TaskRow` contract
as `billed_to`, rather than being looked up from `account` on demand: a slot id
means nothing to a reader who has not saved that login, and the box's own login
can be switched under a run that is still going. A run is **cross-billed** when
that email differs from the dispatcher's `viaUser` claim; two unknowns read as
"not cross-billed", because an unattributed dispatch names nobody to have
wronged.

**`[defaults] billing_guard = "warn" | "require-own" | "off"`**, per-route
override, parsed like `max_duration` — an unrecognised value is refused by
`validate` rather than falling back silently, since a typo would read exactly
like the default and un-arm a route somebody deliberately set to `require-own`.

`warn` (the default) says so everywhere and releases anyway:
- both pickers mark a selection that cross-bills with a warning treatment and
  the text *bills brede@tally.no* — **including row 0**, which is exactly the
  chip an enter-enter release lands on without anybody having chosen it. Row 0's
  effective slot is the route's account, resolved against the host's own
  `ListAgentAccounts`, because `Route default · 8f2c1d0a` answers nothing;
- `comet-board dispatch` / `retry` print one line before releasing — *this run
  bills brede@tally.no's Claude — pass --account <your slot>*. The CLI resolves
  it itself rather than reading it back off the reply: by the time `DispatchTask`
  answers, the worktree is cut and the agent is running on that account. This is
  also why the CLI now sends `viaUser` (from the local `AuthStatus`) — it is a
  frontend like the other two, and without it the guard has nothing to compare;
- the upstream dispatch comment appends *· on brede@tally.no's subscription*, so
  the record is public to both parties instead of living on one usage page;
- `row_metadata` appends *· bills brede@tally.no* for the attempt's whole life —
  outside the per-state arms, because a fact that survives the row changing
  section does not belong inside the match on which section it is in.

`require-own` refuses instead, in `handle_dispatch` **beside the concurrency
cap** — before any attempt row exists, because a refusal that left a `failed`
attempt behind would cost the operator exactly the cleanup this mode exists to
avoid. The override has to *name* the payer: `--bill <slot>` (which also selects
the account) or `--bill <email>` (the only spelling available when the login is
the box's own and has no slot id). In the panel the confirm is reactive — the
mode lives in the host's `routing.toml`, which the panel does not read, so the
only honest way to ask "do you mean it" is to ask after the box has said it
minds. The refusal carries `view::board::REQUIRE_OWN_REFUSAL` so the panel can
tell it from every other dispatch failure without parsing prose.

**This shipped as a seatbelt, not a lock**, and every surface said so in the
words that stay true afterwards. The match was claim-vs-slot-email: a frontend
willing to misreport its signed-in user walked straight through `require-own`,
because relayed board calls arrived as the device room's owner (§gh#55) and #66's
verified identity was what would change that. It was worth having anyway — the
failure it exists for is nobody noticing, not somebody attacking. **§gh#161 made it
a lock over the relay** and left it a seatbelt on the box itself, which is the
one surface where a claim is all a local shell can be asked for. `doctor` reports the mode
the way it reports the notices, never failing, and worded so `off` reads as the
choice it is on a box where one person's plan pays for everything.

Deliberately not here: token or cost caps (§gh#70's note still stands — those need
per-run accounting the harnesses do not expose), and inferring an account from
the WorkOS user who dispatched. The guard *compares* the claim; it still never
authorizes on it, and which subscription a run spends stays the explicit
`account`.
