# The cutoff after tool-calls was a settled mid-loop message — **done** (gh#600)

"Using opencode often cuts off after tool-calls": the model answered, tools
executed, and then nothing — no completion message, the chat idle or dead.
This is the third ticket in the family (gh#37: Idle mid-stream read as
Completed; gh#46: total request deadline closing a busy feed), and it is the
sibling neither fix covers: the turn dies *between steps*, after the current
step's assistant message has already settled.

### The live capture

Driven against the installed 1.18.21 (`opencode serve`, `/event` recorded raw,
2026-08-23). A two-tool turn plays, per step:

```
part.updated w5EP4n status=completed tool=bash
message.updated msg_24g257 completed=true finish="tool-calls"   ← step settles, MID-LOOP
session.status type=busy
message.updated msg_0PZwHg completed=false                      ← next step begins
…
message.updated msg_Fsh9Nm completed=true finish="stop"          ← only the real final step
session.status type=idle + session.idle                          ← turn end
```

Two facts pin the bug:

1. **Every mid-loop message settles.** opencode stamps each step's message
   `time.completed` plus `finish:"tool-calls"` the moment its provider stream
   ends — before the tools run to completion and before the follow-up message
   exists. Only the genuinely-final message carries `finish:"stop"`.
2. **The provider really does die in that window** on this box. The server's
   own log for 2026-08-23 is full of it:
   `stream error … AI_APICallError: Upstream request failed: Endpoint is
   unavailable.` and `Provider finish_reason: network_error` on comet-spawned
   sessions.

So the cutoff sequence the harness receives is: tool results forwarded →
`message.updated` (completed, `finish:"tool-calls"`) → nothing → `idle`. The
gh#37 guard only rejects an idle whose current message never completed; here
the message HAD completed, so the run was reported `Done { Completed }` — the
clean-looking stop.

### The fix

Two independent witnesses that the turn ended mid-loop
(`turn_ended_mid_loop`, `crates/harness/src/opencode/mod.rs`):

- the settled message's `finish` reason is `"tool-calls"` (server's own
  account), or
- the last thing forwarded was a tool result with no text/reasoning delta
  after it (`spoke_after_last_tool_result`; needs no server support).

Either one at an idle — or at the EOF reconcile, where gh#23's "settled
message + closed feed = clean" contract would otherwise swallow it — now
reports `Errored`, with wording that says the stream stopped right after tool
execution and why. A `finish:"stop"` settle, or a turn that never touched
tools, still completes exactly as before; the crash probe keeps precedence in
both paths (gh#79).

Regressions in `crates/harness/tests/opencode.rs` drive the fake serve through
both witnesses' shapes (`scenario:cutoff-after-tools`,
`scenario:stream-stop-after-tools`); the legitimate-end fixtures now stamp
`finish:"stop"` so the distinction is exercised, not assumed.
