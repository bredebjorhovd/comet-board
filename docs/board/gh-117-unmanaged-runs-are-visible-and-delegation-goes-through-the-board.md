# Unmanaged runs are visible, and delegation goes through the board — **done** (gh#117)

*(Since gh#123 — §gh#126 — this group and §gh#103's draw as one **Active** section;
every rule below is unchanged, minus the header.)*
The first real orchestrator session asked for two agents in a space, and the
orchestrator raised two of its harness's *own* in-chat subagents inside its run
instead of dispatching. Work was genuinely running on the box — editing a repo,
holding checkouts — with zero presence anywhere: no attempt rows, so §gh#103's
Agents section drew nothing; no caps, no billing chips, no settle tracking. The
operator's question was "are they even alive", and the only answer was `pgrep`
over ssh.

Two halves, because the hole was two holes.

**Presence for every working chat.** All three sidebars grew a second group,
**Running**, under the Agents: any chat the session watch calls `Working` or
`AwaitingInput` that is *not* a live board attempt — the pinned orchestrator, an
ad-hoc agent chat, anything somebody started by hand. It is the same join §gh#103
does, minus the board row, which is why it costs nothing: the session watch
already streams a status for every chat.

- **`comet_proto::view::board::running_rows`** is the derivation, shared like
  `agent_rows` beside it (and hand-ported to Swift in `BoardModels.swift`, as
  the iOS half of §gh#114 is). Membership is the live indicator and nothing else,
  staleness-gated through `effective_indicator`, so the group fills within one
  watch frame of a run starting and empties within one of it stopping. **No
  board is required** — a box hosting none subtracts nothing and shows its whole
  live list, which is the case the group matters most in.
- **The two groups partition the box's load.** A chat claimed by a
  `working`/`blocked` row belongs to Agents, which knows its issue, branch, cap
  and bill; drawing it in both would double-count what is running. The
  subtraction reads the board rows directly rather than `agent_rows`'s output,
  so a claimed chat stays out even in the case that drops it from the other
  list.
- **The row says only what is knowable**: the chat's own title (there is no
  issue behind it), elapsed since the *run* started off the session mirror —
  not since the chat was created, which for a long-lived orchestrator is days —
  and a blocked badge in words, since no identifier is there to recognise it by.
  One line, not two: an agent row's sub-line carries its branch, and this has
  none. No cap, because nothing bounds a run the board never released.
- **Staleness has to expire the row, and staleness arrives as no frame at all.**
  A backend that died mid-run sends nothing ever again. The TUI rebuilds its
  rows on the counter tick rather than only on updates, and the desktop's board
  ticker redraws once more after the last live thing goes quiet — without that
  the frame the row is gone from is never painted.
- **Not counted: subagents.** The brief asked for "· 2 subagents" on a row if
  the harness stream exposes it. It does not: the Claude normalizer
  (`crates/harness/src/claude/normalize.rs`) drops every frame carrying a
  `parent_tool_use_id` before it leaves, deliberately (a background Task runs concurrently with
  the parent's text stream and folding it in would split a contiguous text
  block). Nothing about a subagent reaches the session mirror, so counting them
  would mean a new streamed field through harness → engine → doc → three
  frontends. Left out rather than faked.

**The brief teaches tickets-first.** `docs/orchestrator.md` said never dispatch
speculatively and never said the inverse, so an orchestrator obeying it to the
letter could still bypass the board entirely. It now names the rule (work you
delegate goes through a ticket; `comet-board new "title" --dispatch` is one
line) and the anti-pattern explicitly: in-chat subagents are for reading, and
anything that lands a commit is a ticket. The same paragraph is in
`docs/agent-conventions.md`, which is the canonical text every runtime gets —
the anti-pattern belongs to every dispatching agent, not only the pinned one.

Deliberately not here, on §gh#103's rule: acting on a running row. Opening it opens
the chat, which is where answering it happens. A glance that can kill an agent
is a glance nobody trusts — and these rows have no attempt to cancel anyway.
