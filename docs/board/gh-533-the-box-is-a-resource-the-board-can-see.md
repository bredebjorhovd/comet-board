# gh#533 — the box is a resource the board can see

gh#529 stopped the engine dying. On the night of 2026-08-19 the kernel's OOM
killer reached inside `comet-native.service` four times, and systemd's default
`OOMPolicy=stop` finished the job each time: one overweight agent build took the
engine, every warm session and the pinned dispatcher chat with it. PR #532
shipped the unit lines that scope the blast radius to the process the kernel
chose — `OOMPolicy=continue`, `MemoryHigh=75%`, `MemoryMax=90%`.

This is everything that night needed and that commit deliberately did not do.
Four things, all of them about the same fact: **the board could not see the box
it was dispatching onto.**

### The cap counts slots; a slot is not a memory budget

`max_concurrent_per_workspace` bounds how many agents run at once. It cannot
bound what they run *into*: three slots of Next.js builds is not three slots of
doc edits, and the Mylder rule held a swapless 16G box at 3-of-3 heavy builds —
passing the capacity check every time — until the kernel started choosing
victims.

So there is a second meter, measured on the box at the moment of the dispatch:
`pressure.rs::headroom`. `MemAvailable` against `[defaults]
min_memory_headroom` (15% by default, `off` to disable), plus PSI `some avg10`
for the box that is already thrashing whatever it claims is free — available
memory looks fine right up to the moment reclaim stops keeping up. The floor is
a percentage rather than a byte count so one default fits a 16G VPS and a 64G
workstation, and it is board-wide rather than per-route because routes cannot
have their own budget of a resource they all spend.

Two callers, because there were two ways in:

- `dispatch.rs::check_pressure`, beside `check_capacity` in the engine's
  `handle_dispatch` — every keypress, every CLI dispatch, every relayed call.
- `EvalFacts::host_pressure` in the auto-pick planner, which is the caller this
  exists for: the automation refilling a slot a settle just freed is the one
  nobody is watching.

The word is **deferred**, in gh#490's vocabulary — eligible-but-held by a limit
that time will lift, which is exactly what a build finishing does — and the
reason is the board's own sentence with the numbers in it. The planner asks the
box *after* the caps: a task whose space is full is held by a rule somebody
wrote, and only a task with a free slot waiting for it is being held by the box.
Reporting the memory reason against every ineligible row is how the one line
that matters gets buried.

One reading per evaluation, taken with the other facts and handed down, so every
rule in one tick is judged against one box — and so the settings page's preview
shows the sentence the loop would record.

### A box that cannot be measured is never the reason work stops

Everything in `pressure.rs` is read from `/proc` and `/sys/fs/cgroup`, so
everything is Linux. On macOS every reading is `None`, `headroom` returns
`Unknown`, and `check_pressure` releases. That is not a gap to fill later: a
gate that refused what it could not see would stop work on the box the operator
is sitting at, and a board that guessed at numbers it did not take would be
inventing register data about its own machine. Every field is an `Option` and
every `None` abstains.

### A killed child is a loud event

With `OOMPolicy=continue` the engine now survives the kill — which means the
only trace left of it is one agent's death, and "claude exited unexpectedly
(killed by signal 9)" is a sentence a person reads as a flaky CLI. It was read
as the phone being flaky, four times, that night.

`OomWatch` is armed when a run starts (`sessions.rs`, beside the turn
guardrails — that task owns the run's whole life) and asked when its child dies.
The counters are cumulative, so only a delta across one run means anything:
`memory.events`' `oom_kill` for the engine's own cgroup, and `/proc/vmstat`'s
boxwide counter, which answer different questions — a boxwide count that moved
while the cgroup's did not is the box killing somebody else.

The rewrite happens on two conditions, never one: the child died on signal 9 or
15 **and** the counters moved. Signal 9 alone proves nothing — a `kill -9` from
a shell is byte-identical — and a board that guessed would tell an operator
their box is out of memory every time somebody killed a stuck CLI by hand.
Signal 15 is included because that night produced both: with the pre-gh#529
`OOMPolicy=stop`, systemd answered one kernel kill by stopping the unit, and
every other child died on a SIGTERM that had nothing to do with its own memory
use. They died of the box running out of memory too.

What lands in the chat is the claim with its evidence in the same sentence, and
the harness's own message kept behind it — on the day the attribution is wrong,
that is the only way to find out.

One thing had to be fixed underneath it. Both the claude and codex adapters
described a crashed child with a bare `try_wait()`, which across the window
between stdout reaching EOF and the child becoming reapable reports "still
running" — a status with no signal number in it, which no attribution can
rescue. `settled_exit` waits instead, briefly and boundedly, the way the
opencode adapter already did.

### A fix can ship and not arrive

gh#529's three lines are written when a unit is *rendered* — at install, or by
`comet daemon install`. An engine update rewrites the binary and nothing else,
so every box installed before that release kept `OOMPolicy=stop` and kept dying
for it. The production box got the lines by hand on 2026-08-20, which is not a
distribution mechanism.

They now ship as a **drop-in** (`comet_update::service`), re-asserted on every
engine update immediately before the reload and restart that make it take
effect. A drop-in rather than a re-render because the rendered unit carries the
environment captured at install time and the `ExecStart` of whichever installer
wrote it; regenerating that from an updater — which knows neither — would
replace a working unit with a plausible one. It is content-addressed, so an
up-to-date box is not churned; `50-` so it loses to an operator's own `90-`
override; and it says in the file itself how to opt out.

`comet daemon`'s renderer and the drop-in share one constant, so a freshly
installed box and an upgraded one cannot end up on different settings.

### doctor says what the box is and what it has already killed

Four lines, and only one of them is red:

- **host memory** — the reading, the floor, and which side of it the box is on.
  Never red: a box that is momentarily tight is a box that is *working*, and a
  line that goes red every time three agents are building stops being read.
  This is the answer to "why is nothing being released".
- **swap** — a warning, with the whole `fallocate … mkswap … swapon … fstab`
  command, because the box where it fires is a headless VPS being read over ssh.
  A warning and not a failure: plenty of boxes are swapless deliberately. What
  it is not allowed to be is silent — swaplessness is why the kernel's only
  available response that night was a kill.
- **load** — the fifteen-minute average against the core count, because the word
  in the question is *sustained*. Corroboration, not cause.
- **oom kills** — red, with dates. The only one of the four that reports
  something that has already happened, read from the unit journal over a week
  rather than from the counters, which reset every time somebody restarts the
  engine to make the problem go away.
- **engine unit** — red when the unit would still die with its child, printing
  the paste that fixes it. The check that exists because a fix can ship and not
  arrive.

### What is not proved by a test

No test may fill a CI runner's memory to make a real OOM killer choose. The
harness regression (`scenario:oomkill`) kills its own child with SIGKILL the way
the kernel would, and proves the two halves that are ours: the supervisor
survives, and the death is reported with the signal number the attribution is
read off. Everything downstream of that signal — the counter delta, the
sentence, the message that reaches the chat — is proved where it is decided, in
`pressure.rs` and `sessions.rs`, against injected counters.
