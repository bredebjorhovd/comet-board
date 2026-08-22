# One board, two machines — **the code, done; the cutover, gated** (gh#558)

Two boards on one account, partitioned only by which repos each polls. The Mac
(`McComet`) polls `bredebjorhovd/comet-board` and `bredebjorhovd/itsm-agent`;
the box (`Tokenmaxxer9000`) polls the eight Tally-side repos. `doctor` is
explicit that the partition is the only safety there is:

> nothing they poll is also polled here, which is the only thing keeping them
> out of each other's work

And the phone can only ever show one of them. `BoardStore` sweeps the candidates
and settles on the first with dispatch evidence, deliberately (gh#125), so the
operator sees the box and nothing of the Mac's.

The target: **the box hosts the single board.** Not because of where work runs —
routes decide that — but because the Mac sleeps, and a board on a sleeping
laptop stops polling and stops dispatching. The Mac keeps its
`comet-board-laptop` space and still runs every Rust/iOS/macOS job, because
comet-board has iOS and macOS targets that cannot build on Linux. It just stops
hosting a board.

### It was not a config change

The ticket's reading was that this needs no code: a route names a space, a space
is a device+folder pair, and the board's space lookup never filtered to the
local device —

```rust
// crates/engine/src/board.rs
let found = spaces.borrow().iter()
    .find(|s| space_matches(s.name.as_deref(), &s.path, &route.workspace))
    .map(|s| SpaceRef { id: …, device_id: s.device_id.clone(), path: … });
```

— so `DispatchSpec` gets the Mac's `device_id`, and all seven spaces are already
visible from either device through the workspace doc.

That much was true, and more of the path than expected already followed it.
`WorkspaceHost::create_chat` stamps the chat with `space.device_id`, so the
conversation, its command ledger and its runs belong to the space's own machine.
Settle reads `AgentStatus` off the *merged* session watch, which carries remote
devices' rows. Review delivery queues into the chat's own doc, wherever it is.

**The checkout did not.** `CometRuntime::dispatch` cut it with the local
`Repos`, against `spec.repo_path` — a path that exists on the other device. On
the real fleet that is `/Users/brede/dev/comet-board` read from a Linux box, so
the box would have refused every comet dispatch with a git error about a
directory it does not have. Nobody had run a cross-device dispatch, so nobody
had seen it.

Two smaller things went the same way, and for the same reason — they are
per-device state prepared by whichever machine happened to host the board:

- `AgentAccounts::materialize` — a slot id is portable, the `~/.claude` behind
  it is not. The box would have seeded its own login for a run on the Mac, and
  failed the dispatch outright over a slot that is present where the run
  happens.
- `conventions::apply` (gh#272) — the instruction file the runtime reads without
  being asked, written into the config dir the run will use. Same argument.

### The fix

One new relay-forwardable verb, `CreateBoardWorktree`, and a branch in the
dispatch path that takes it when the space is not here.

It is distinct from `CreateWorktree`, which generates the branch name and treats
its `branch` param as a *start point*. This is the board's cut: the branch name
comes from `routing.toml`'s `branch_template` and is exact, and a fresh branch
starts at `base` fetched from origin (gh#67).

It carries the agent-config half as well — harness, account slot, and the
`agent_instructions` flag — and the device about to run the attempt materializes
its own login and writes its own conventions file before it answers. That is the
whole of what "prepare the ground" means, moved to the machine the ground is on.

Everything else is unchanged, because everything else was already a workspace-doc
write that lands on the space's device.

When the board has no relay — signed out, or the edge down — a cross-device
dispatch is refused as a `RefusedBeforeCut` **naming the device**, so no attempt
row is burned and the operator's next move is "bring the edge up" rather than
"make a directory".

### `doctor` stops lying about the other machine

`doctor` validated route spaces against **local** spaces only, so a route
pointing at the other machine read as `no comet space named` — with a repair
that would have cloned a second checkout onto the wrong box. The Mac's
`itsm-agent` route has been failing for exactly this reason the whole time: that
space exists, on the box.

The space list handed to `doctor` is now the workspace's whole list, with the
local device id beside it, and three answers instead of two:

- **here** — `` `comet-board-laptop` exists ``, and every disk check under it is
  about a real path.
- **on another device** — named, not an id: `` `comet-board-laptop` is on
  McComet, not here — this board dispatches there ``. Not a failure. The `repo`
  line reports the path and says it was not checked from here; the `base` check
  is not run at all, because it could only ask the wrong disk.
- …and on a remote route, the `repo` line compares itself to the space's own
  folder. `repo =` is the path a dispatch cuts in — `build_spec` reads the
  route, not the space — so on a remote route it has to be a path on *that*
  machine. This box cannot stat it, but it can notice the two disagree, which is
  what a route left behind by the board that used to run here looks like. Run
  against the live Mac, that is the first thing it found:

  ```
  ok route itsm-agent: space `itsm-agent` is on Tokenmaxxer9000, not here …
  ok route itsm-agent: repo  /Users/brede/.comet-native/repos/itsm-agent — but the
     space is at /home/comet/.comet-native/repos/itsm-agent on Tokenmaxxer9000, and
     `repo =` is the path a dispatch cuts in. One of the two is wrong, and neither
     is on this disk to check
  ```

  That route would have dispatched into a directory that exists on neither
  machine. It goes away at step 1 of the cutover; until then it is at least
  visible.
- **nowhere** — the failure it always was, with `have:` still listing *this*
  device's spaces: offering to adopt another machine's folder is not a repair
  anybody can run from here.

`init` and `adopt` keep the filtered list. Both probe folders on this disk.

### The proving step

`crates/engine/tests/board_cross_device_dispatch.rs` — a whole test binary,
because the only way to prove *which machine* cut a checkout is to give the two
engines different worktree roots, and that root comes from a process variable
read once inside `Repos::new`.

It asserts the two facts that matter: with no relay the dispatch is refused by
name and nothing is cut anywhere; with the relay up, the checkout lands under
the Mac's worktrees root, the box's root is never created at all, the path is a
real checkout, and the chat is stamped with the Mac's device id.

Reverting the dispatch branch turns it red with the box cutting into its own
root — which is the bug, in the one place it is visible.

### The cutover — not done, and deliberately

The config half of the ticket is **not** in this change, and the ordering is the
dangerous part of it:

1. **Remove `comet-board` and `itsm-agent` from the Mac's `[github] repos` and
   its routes first.** If both boards poll one repo even briefly they derive the
   same issue as ready and either can dispatch it — two agents, two branches,
   one ticket. This step must not be reordered. (`comet-board onboard` refuses a
   repo another board already polls, gh#343, but that guard fires at the *add*,
   which is step 3.)
2. Add both repos to the box's routing, with `comet-board` routed to the Mac's
   `comet-board-laptop` space and `itsm-agent` to the box's own checkout at
   `/home/comet/.comet-native/repos/itsm-agent`.
3. Retire the Mac's board. Retire it by emptying its `[github] repos`, not by
   turning the board off: `PushCredentials::detect` is wired only when the
   engine runs with a board, and a Mac with no board at all has no credential to
   hand the agents it is still running.
4. `comet-board doctor` on both hosts. `board hosts` must report no overlap, and
   every route must resolve — including, on the box, the comet-board route
   reading as "on McComet".

**Gated on gh#557.** The box learns about the Mac's spaces through the workspace
doc, over the same room that is currently poisoning its wasm heap and resetting.
A board that cannot see the Mac's spaces refuses every comet dispatch with `no
comet space named` — the correct refusal, for a reason that has nothing to do
with this. Land the code, leave the config until the room is stable.

### What is still device-blind

Two checks in the dispatch path ask the board's own device a question that
belongs to the space's:

- **`harness_availability` (gh#187)** — "can this harness start here?", asked of
  the box for a run on the Mac. Both machines have `claude-code`, so it does not
  bite today; it would bite the first time a route named a runtime only one of
  them has, and it would bite in the wrong direction (refusing a dispatch the
  target could run, or admitting one it cannot).
- **`Runtime::reclaim_worktree` / `reclaim_build_output` (gh#72, gh#186)** — the
  GC sweeps checkouts on the device running the board. A cross-device attempt's
  worktree is on the other machine, so nothing reclaims it and nothing counts it
  in `doctor`'s disk lines. The Mac's own board used to do that sweep; after the
  cutover, no one does.

Neither blocks the consolidation. Both are the same shape as the bug this
ticket fixed — a device-local answer to a question about another device — and
both want the same treatment: a forwarded call, or an honest "not checked here".
