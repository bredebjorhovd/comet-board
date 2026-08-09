# "Hosts no board" and "could not be asked" are different answers — **done** (gh#155)

"if i click add space and try to search for repos I cant find it." Nothing was
misconfigured. §gh#118's sweep asks every device `ListRepoSpaces` and skips the
ones that fail, on a contract that is sound as far as it goes: a device hosting
no board refuses before it does any git or GitHub work, so being refused rules
it out for free. What the contract could not survive was a host that *would*
have answered and could not — during §gh#126's free-tier Durable Object outage
every relayed call to the box returned 500, so the box was skipped exactly like
a laptop that hosts nothing, and the picker showed the Mac's repos with no
error, no spinner and no hint that a second device had been asked. The shorter
list read as the whole truth. §gh#126's lesson, on a surface that never learned it.

- **The two answers were the same `Err`, and that is the real fix.** Every
  board-addressed method funnels through `EngineRpc::board()`, which returned
  `RpcError::Failed` — indistinguishable at the call site from a dead relay.
  There is now `RpcError::Refused`, and it survives the hop: `ServerFrame`
  carries an optional `code` beside `err`, `RpcError::code`/`from_wire` are the
  two ends of it, and an untagged frame (an older peer) still reads as
  `Failed`. Nothing else changes on the wire, and the `Display` text is
  unchanged, so no existing message moves.
- **Silence is now a fact the picker holds.** `hosts_no_board` is the whole
  rule, one predicate with a test: `Refused` and `UnknownMethod` are *answers*
  and rule a device out silently; transport, `Closed` and everything else mean
  nobody was asked. The unasked devices land in `AddSpaceFlow::unreachable`,
  named — with the transport's own words underneath — and the list keeps every
  repo that did answer, because the ones that answered still work.
- **The warning sits above the list, with Retry.** `view::repos::unreachable_note`
  is the pure sentence ("Could not reach box — any repos hosted there are
  missing from this list."), named devices rather than a count, because the
  operator's next move is device-shaped. A footnote under a plausible list is
  how this was missed the first time. The rail stops claiming "no device here
  hosts a board" when the sweep never got to make that claim, and an onboard
  with nowhere to go says which of the two is true.
- **Absent is not unreachable, or the strip is on every time.** The sweep asks
  every registered device, and this fleet is Mac + box + iPhone: a phone is
  asleep essentially always, hosts no board and never will, so its relayed call
  fails as transport like any other. Warning about it on every open is how a
  strip stops being read before the day it has something real to say — silent-
  when-it-should-speak was the bug, and loud-every-time is how the fix gets
  reverted. So the report is gated on §gh#126's presence verdict, gathered per
  device *before* the call (silence carries no facts): `Offline` — a lapsed
  heartbeat seen by a viewer whose own sync is up — is absence, and gets one
  muted line and no Retry, or nothing at all when the device holds no spaces
  here and its silence therefore costs the list nothing. `SyncDown` is
  deliberately not absence: a viewer that cannot hear does not get to call
  anything away, and that is precisely the state the outage put the box in, so
  the case this issue is about still gets the full treatment. `Candidate::silence`
  is the rule, with the fleet's own shape as its test.
- **The sidebar's copy of the sweep had the same disease.** `refresh_space_slugs`
  replaced the `space → owner/repo` map wholesale from every sweep, so one
  unreachable box quietly renamed every one of its spaces back to a folder
  basename. A sweep every device answered still replaces; one that lost a device
  merges (`AppState::merge_space_slugs`) — it can add and update, never delete.
- **Not done: the phone.** `BoardStore.repoHosts()` swallows the same way
  (`guard let … try? await` — one `continue` for both answers), and the phone is
  the surface where the repo list is the *only* door. The wire now carries what
  it needs (`code` on the frame); porting it is `RelayError.refused` plus the
  banner, and it is a separate change with a separate build to verify.
