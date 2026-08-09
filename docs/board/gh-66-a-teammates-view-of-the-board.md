# A teammate's view of the board — **done** (gh#66)

A second person in the org gets there through three org gates, all at the edge:

1. **They see the box.** Device rows are published to an org-wide registry
   (`orgdev1/{orgId}`) alongside the per-user workspace doc, so `WatchDevices`
   on a teammate's laptop lists the box — which is what the pane's host sweep
   walks to find a board at all.
2. **They may relay to it.** A device room admits any member of the org that
   claimed it (as a client; only the box's own backend may host it), so the
   forwarded board RPCs reach it.
3. **They may open its chats.** A dispatch marks its chat shared with the org
   (`POST /share/{chatId}`), which is what lets a teammate open the transcript
   and steer the agent. Chats nobody shared stay private to their owner, board
   or not — being in the org does not make someone's own sessions readable.

The chat still RUNS on the box: its session doc names the hosting device, so a
teammate's engine syncs and writes the doc (a steer is a command entry in it)
without ever executing the work itself.

Those three gates are what a teammate *may* do; `docs/teammate.md` is what
somebody has to actually set up for them, in order — the page §gh#162 added
because the answer had until then existed only as `doctor` output nobody reads
before the fact.
