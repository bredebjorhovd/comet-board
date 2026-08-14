# Two resume budgets that cannot see each other — **done** (gh#392)

Found reviewing gh#390, and not a defect in it: it is what happens when the
budget that change added meets one that was already there.

### Two counters, neither aware of the other

**The engine's**, since long before gh#390. `Sessions::recover_stale` runs on
boot: for every journal whose last event is not `Done` it stamps the abandoned
entries `aborted`, closes the journal with a synthetic `Done{interrupted}` —
and re-dispatches the run against the remembered harness session if it has
budget. `MAX_AUTO_RESUME = 3`, counted in a file beside the journal.

**The board's**, added by gh#390. `runs::MAX_RESUMES = 3`, counted in
`attempts.resumes`, spent when a chat is `Alive` with no run.

On the ordinary path they are disjoint. The engine acts on a **stale** journal;
the board acts on a chat whose journal is **closed** and which has no run. Two
different states, no overlap.

They meet at exhaustion. An engine that runs out of revivals *still* closes the
journal with `Done{interrupted}` and simply stops re-dispatching, which leaves a
chat that is alive, a journal that is closed and no run — exactly
`Liveness::Alive` with no run, so the board began resuming **from zero**. One
attempt's run could be started six times: three by the engine, invisibly, then
three by the board.

### Why the number matters

`runs::gave_up_note` is the one sentence a person reads when the board finally
stops, and it says *"its run died 3 times and was restarted every time without
finishing — the box is not keeping runs alive"*. The number is the evidence in
that sentence. It also carries the reasoning behind the constant, which is
otherwise sound: an engine restart takes one resume, a second in the same
attempt is an ordinary evening on a box somebody is updating, and a third
failure inside one attempt is no longer bad luck. That argument is about how
many times a thing has actually failed, and three prior failures nobody counted
make it false.

gh#390 exists because the board said something confident and wrong.

### The fix: one budget, read from both ledgers

`Runtime::chat_revivals` — the board already asks the runtime `chat_alive` on
exactly this path, and how many times the engine has revived this chat is the
missing half of "how many times has this actually been started". The live engine
answers off the same journal directory every other run fact comes from; the
count is the engine's own, written by the engine's own boot recovery on this
device.

`runs::Restarts { board, engine }` carries both. `spent()` is what the budget is
weighed against, so one budget governs wherever the restart came from: an engine
that spent three leaves the board none, and an engine that spent one leaves it
two. `counted()` is what a note may put in front of a person, and the note names
the engine's share separately — those restarts leave nothing on the board at
all, so a reader comparing the total against `attempts.resumes` would find them
missing and trust the smaller number.

**`None` is not zero.** An unreadable ledger, no runtime this cycle, or a chat
this device has never run all answer "cannot say". Then the board spends nothing
extra and restarts on its own count exactly as it did before — `Liveness::
Unknown`'s rule applied to the other half of the same state — and `gave_up_note`
drops the number rather than claiming one it cannot support: *"its run kept
dying and was restarted until the board would restart it no more"* is honest;
*"3 times"* would not be.

The disjoint path is untouched: an engine restart with budget left revives the
run itself, the board sees a chat that is working, and no board resume is spent.

### What is not here

The engine still does not read the board's count, and it should not — a boot
recovery that had to open `board.db` to decide would couple the two the wrong
way round, and the engine revives chats the board has never heard of. So the
board cannot *pre-empt* a revival: an engine that restarts three times while the
board is between ticks will revive three times whatever the board has spent.
What the board can do about a restart it does not perform is count it, decide
with it, and say it — which is now all three.

- the decision and both counts: `runs::Restarts` / `runs::decide` /
  `runs::gave_up_note` in `crates/board/src/runs.rs`
- the question: `Runtime::chat_revivals`, answered by `CometRuntime` off
  `RunJournal::revivals`
- the reader: `SyncEngine::restarts`, feeding `resume` and `give_up` in
  `crates/board/src/sync.rs`
- the engine's half, unchanged: `Sessions::recover_stale`'s `MAX_AUTO_RESUME`
