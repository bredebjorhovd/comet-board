# The dispatch comment names what ran, and who — **done** (gh#232)

The line the board leaves on an issue at dispatch was wrong about two of the
four things it said, and it is the part of a dispatch that outlives the chat,
the row and the worktree. Seen on gh#222 and gh#223, the first two dispatches
this board made to a runtime other than `claude-code`: both read

```
Dispatched to comet · claude-code · space:comet-board · attempt 1 ·
dispatched by f31135c6-92d2-4efa-a0c1-1c740170f4c7
```

when what ran was `opencode`/`deepseek-v4-flash` and `codex`/`gpt-5.6-luna`,
released by an address the board was holding at the time.

### The runtime, and the model beside it

`handle_dispatch` passed `&route.runtime` into `enqueue_dispatch` while the
spec, the attempt row and every view took `overrides.runtime` — so a
`--runtime` override was dropped on exactly one surface, and the one nobody
could correct afterwards. The writeback now takes `RanOn { runtime, model }`
rather than a bare `&str`: the pair is one answer, and a `&str` parameter is
what let the route satisfy it silently. A dispatch under an override
typechecked while reading `claude-code` upstream.

The model is new. For a board spreading work across harnesses it is the more
useful half — `codex` says what ran, `codex · gpt-5.6-luna` says what the next
attempt should be compared against. `None` is the harness default, which the
board cannot spell and so does not name: no model in the payload, no segment in
the comment.

### A person, not a chat id

`dispatcher_name` resolved `Dispatcher::Agent` to the parent's issue identifier
when the board had dispatched that parent too, and otherwise to the raw chat
id. Because that second arm returned `Some`, the caller's fallback to the human
the frontend named (gh#74) never ran, and a UUID took the place of an address
already stored on the attempt as `dispatched_by_user`.

The order is now identifier → person → chat id, decided inside
`dispatcher_name` rather than half there and half at the call site. A chat id is
legible exactly when it resolves to an issue: when it does not, it is the least
useful of the three facts on hand, and it belongs where it was always meant to
be — the last resort, for an agent-issued dispatch with nobody signed in. A
blank or whitespace claim is nobody, and falls through to it.

`Dispatcher::Operator` returns the person too, which is the same fallback the
caller used to apply; nothing about who is *billed* moved (gh#101), and nothing
about strength (gh#161) reaches this line — a public issue comment's audience is
not the party that cares whether the box verified the name.

### One sentence, one function

Linear and GitHub each had their own copy of the same `format!`, so a fix to
either was a fix for half the board's readers. Both now call
`dispatch_comment(&payload)`, tested directly against the payload the queue
holds — which is where this diverged from every other surface, and so is where
the test has to be.
