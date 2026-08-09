# The board in your pocket — **done** (gh#114)

herdr's answer to "on the phone" was Tailscale plus mosh into a terminal. comet
already had a native iOS app signed in against the edge and syncing chats; the
board was the missing screen, and with it the only surface you can reach from a
parking lot.

**The load-bearing unknown was the transport, and it was real.** `WatchBoard` is
a *stream*, and `DeviceRelayClient` — the phone's ControlRpc client over the
device-room relay — only understood unary `{ok}`/`{err}` replies. An `{item}`
frame fell through to "unexpected reply" and a subscription hung until its
deadline, so no engine subscription had ever been reachable from iOS. The client
grew the other half of `comet-rpc`'s client: `subscribe` returns an
`AsyncThrowingStream`, `{item}` yields, `{done}` finishes, `{err}` throws, and
dropping the consumer sends `{id, cancel: true}` so the host stops producing
into a socket nobody reads. `E2ERunner.probeBoardStream` is the regression: on a
device that hosts no board the engine *refusing* is a pass — what it is testing
is that a ServerFrame comes back at all, since a hang is what the old client
looked like.

- **Finding the host.** The desktop asks its own engine to forward with
  `targetDeviceId`; the phone has no engine, so it dials each candidate's device
  room directly and calls the same relay-forwardable method there. Same sweep,
  same rule for ruling a candidate out (a stream that ends without ever
  delivering a frame said "not me"), minus the `None`-is-this-device entry that
  a viewport with no local board cannot use — `boardHostCandidates`.
- **The subscription is standing**, not opened with the board screen. gh#103
  made that correction on the desktop for the same reason: the Agents section on
  Home is presence, and presence that only works after you have visited the
  board is not presence.
- **The view derivations are ports, not approximations** (`BoardModels.swift`
  cites each one): section order and glyphs, `finished_today`'s local-midnight
  bound on `done`, `format_elapsed`/`format_cap`, the `billed_email` /
  `cross_billed` / `bills_label` vocabulary, and `agent_rows` whole. What is
  deliberately not ported is terminal layout — `row_metadata`'s fixed-width
  column block pads to a monospace grid that does not exist here, so the
  *content* decisions inside `state_metadata` come across as `BoardRowDetail`
  and the row view lays them out.
- **The dispatch sheet asks two questions**, runtime and account, and the
  account one matters more here than anywhere else: on a desktop the picker is a
  popover you arrow through, on a phone enter-enter is a thumb tap, and the row
  a tap lands on without anybody choosing it is the route's default — exactly
  the release that can quietly spend a teammate's subscription. So the chips
  resolve to emails rather than slot ids, and the sheet opens at the large
  detent because a detent that hides the account row is a picker that always
  lands on the default. A `require-own` refusal (gh#101) comes back as the
  confirm the CLI spells `--bill`, never as a dead end.
- **Deliberately not on the phone**: the model picker (a dispatch with no
  override runs the harness default, which is what the route already meant, and
  nobody changes models at a bus stop) and the `f`/`/` filter cycle (a keyboard
  affordance). Cancel is behind a long-press on the board's own rows, on the
  desktop's rule that a glance which can kill an agent is a glance nobody
  trusts.

Verified end to end from the simulator against a real headless box over a
`wrangler dev` edge (`-e2e-board <repo>`): board attached over the relay, rows
arrived by stream, a ready row dispatched and moved to `working` with its branch
cut and `started_at` set, the agent row derived, and `replace` ended that
attempt and released a second one (`attempts=2`).

**Distribution, for the operator.** Personal-device sideload via Xcode free
provisioning works today and re-signs every 7 days. TestFlight needs the $99
Apple Developer account — the same one gh#100's signing tier wants, so it is one
purchase for both.
