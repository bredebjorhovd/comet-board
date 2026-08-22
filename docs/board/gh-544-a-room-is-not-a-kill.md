# A room is not a kill — **done** (gh#544)

`doctor` printed this, in red, on a box whose ground truth was fifteen kills
two nights earlier and a cgroup counter sitting at zero:

```
FAIL oom kills   13460 oom-kill event(s) in the engine's unit journal in the last 7 days — latest 2026-08-20T20:16:18+00:00: …INFO comet_engine::workspace_host: room reconnected room=cd16185d…
```

Both halves of the line are wrong at once. The count is inflated about a
thousandfold — `journalctl --user -u comet-native --since -7days | grep -ciE
"oom.kill|OOM killer"` says **12** on the same box, over the same window — and
the "latest event" quoted as evidence is not an oom kill at all; it is a room
reconnecting.

### What was wrong

gh#533's parser matched on the word, not on any sentence:

```rust
lower.contains("oom") || lower.contains("out of memory")
```

The engine logs `room reconnected` lines by the thousand, and "r**oom**"
contains "oom". A week of ordinary room chatter read as thirteen thousand
kills. And because everything counted, the line shown as "latest" was simply
whatever printed last in the window — so the evidence for an oom incident was
guaranteed to be the engine's most recent log line, which is exactly the thing
you do not want to read during an incident and cannot use after one.

### The rule now

Two rules, both about reading the way the operator reads:

- **Only genuine markers count.**
  [`OOM_KILL_MARKERS`](../../../crates/board/src/pressure.rs) enumerates the
  sentences each writer actually emits: the service manager's announcement old
  and new ("A process of this unit has been killed by the OOM killer", "The
  kernel OOM killer killed …"), its verdict on the unit ("Failed with result
  'oom-kill'"), systemd-oomd ("Killed … due to memory pressure"), and the
  kernel itself ("Out of memory: Killed process 12345 (node)"). The kernel
  needle keeps its colon-and-word tail rather than stopping at "out of
  memory:" because the engine retells every kill it attributes into this very
  journal through tracing (`run killed: the box ran out of memory: …`,
  `sessions.rs`) — and that sentence describes a death the unit's lines
  already counted. Counting it too doubles every incident.
- **The verdict ages, by gh#515's rule.** [`oom_kills_check`] takes `now` like
  `push_verdict` does. A kill inside `OOM_FRESH_SECS` (one day) is news: red,
  with the age leading the quote ("latest 7h ago (…)"). When every kill in the
  window is older, the line goes green and says what makes it green — "Nothing
  has been killed in the last day — history rather than news" — with the
  count, the age of the last kill and the verbatim quote still attached,
  because history you cannot inspect is just a smaller mystery. A timestamp
  that will not parse reads as *fresh*, never as old: a kill nobody can put a
  time on must not be waved off.

### The fixture

`room_chatter_is_not_an_oom_kill_and_the_newest_match_is_what_gets_quoted`
carries the box's actual night — kernel kill line, systemd announcement,
systemd verdict — interleaved with two days of reconnect chatter that buried
it, and pins both the count (3) and the quoted line (the `Failed with result
'oom-kill'` verdict, not the reconnect that prints two days later).

`doctor` is what you run when the board looks wrong. A check that counts rooms
and quotes them as corpses spends the operator's attention on infrastructure
that is fine, which is the failure mode `doctor` exists to prevent.
