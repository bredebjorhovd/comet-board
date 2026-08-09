# The box verifies who dispatched — **done** (gh#161)

§gh#103's guard compared the dispatching frontend's *claimed* `viaUser` against the
email on the agent account a run would spend, and `billing.rs` said out loud
what that was worth: "a frontend willing to lie about who is signed in walks
straight through `require-own`. This is a seatbelt, not a lock." The identity
that would fix it already existed and was thrown away at the door — the Worker
stamps `x-comet-auth-user` on every DO forward from a verified session
(`edge/src/env.ts`), the device room reads it per connection, and then relayed
the frame with `from = connId` and nothing else. So the box received a name and
no way to corroborate it.

**The relay carries the caller.** The client→host frame header grew `u`/`o` —
the verified user and org — stamped by the DO from the socket's own accept-time
identity, next to `from` and by the same rule. `relayedHeader` is a pure
function that takes the client's frame apart key by key rather than spreading
it, because that *is* the security property in one line: a client chooses the
stream and the kind, and nothing else. A frame that arrives with no stamp did
not come through the relay at all — it came from the box's own IPC port, and
that absence is a fact worth as much as the presence.

**The engine prefers it, and says which it got.** `comet-rpc` grew `Caller`
(the transport's answer to "who is this") and `RpcService::handle_as`, a second
method with a default that drops it — nothing wants this but `DispatchTask`, and
a changed signature would have made every handler pretend to care.
`serve_connection_as` fixes one caller per connection; the host relay keys its
virtual connections on `(connId, verified user)` rather than `connId` alone,
because `connId` is chosen by the dialing client and two people can pick the
same string. `DispatchOrigin::attribution()` is then the one place that decides
what the rest of the board sees, and a verified stamp **replaces** the claim
rather than merging with it: `Verified(email)`, `Claimed(email)`,
`Unnamed{user_id}` for a verified caller the box could not resolve, `Nobody`.
The attempt records which (`dispatched_by_verified`), because "we know who this
was" and "they told us" must not render identically — the orchestrator's
released-by line marks the claim `(as claimed)`; the public issue comment does
not, since its audience is the repo and not the operator.

Putting a name to a `sub` is `Auth::email_for_user`: our own session first
(free, offline, and the box has to recognise itself even with WorkOS
unreachable), then the workspace roster. It answers `None` for "cannot say",
never for "nobody".

**`require-own` refuses on three grounds now**, all of them in `Billing::refusal`
and all answerable by naming the payer:

1. the run bills somebody else — the old refusal, on a comparison the box no
   longer has to take anybody's word for;
2. **the dispatcher cannot be named.** A verified caller the roster did not
   resolve is refused, because the alternative is falling back to the claim,
   and the claim is what this mode stopped believing. It says the user id, so
   the refusal is actionable rather than mysterious;
3. **nothing named an account, and the dispatcher is not the box.** A dispatch
   that names no slot spends the box's own CLI login; where the box can name
   that login this is already (1), and where it cannot, (1) goes quiet and a
   teammate's run charges the owner in silence.

The local box is deliberately not collateral damage: (2) and (3) cannot fire on
a dispatch with no relay stamp, because nothing but the box's own processes can
reach its IPC port. An unattributed dispatch is still released by every mode —
it names no wronged party.

**The same failure from the other side** is that `account` is optional and its
absence is quiet. `doctor`'s new `default account` line names the routes that
name none, whose login the fallback actually is, and how many people are in the
workspace — which is the fact that turns a tautology into a warning, and the
only reason it needed the roster (`ListMembers`, best-effort, `None` when
unasked). It never fails, for `billing guard`'s reason: sharing one plan on
purpose is a normal way to run a box.

**Order of landing.** The edge half needs a Worker deploy; the engine and board
halves are inert without it — an unstamped relay reads exactly as today's local
claim — so nothing gets less safe while it rolls out.

Tests: the in-memory room in `crates/engine/tests/device_routing.rs` now stamps
the identity it verified, and two laptops dispatch the same task into one box
under `require-own`, both claiming to be the owner. The teammate is refused and
refused *naming them*; the owner's own laptop clears the guard and dies on the
next refusal. That file also stopped building its fixture board with
`Paths::under`, which honours `COMET_BOARD_*` — under a box that sets them (i.e.
under an agent the board itself dispatched) the tests seeded fixtures into the
live board's database and wrote their fixture over its `routing.toml`.

Deliberately not here: inferring an *account* from the verified user (§gh#103's
note stands — the guard compares, it does not choose), a per-user default
account, and the phone's rendering of the new flag.
