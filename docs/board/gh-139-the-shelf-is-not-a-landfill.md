# The shelf is not a landfill — **done** (gh#139)

"Do all complete sessions just accumulate under the folder?" They did. gh#72
reclaimed the *checkout* an attempt leaves behind and nothing reclaimed the
other half: a board-dispatched chat was archived only by a hand, so at agent
throughput a space's shelf silted up in days and the six chats somebody was
actually working in were somewhere in it.

`[defaults] archive_chats` (per route, `off` honored), swept by
`SyncEngine::archive_chats` beside `collect_worktrees` on the same interval.
Shipped at the checkout's `7d`; §gh#144 made it `on-settle` after a morning spent
reading last night's finished rows.

- **The same rule, not a second one.** `gc::chat_standing` is `gc::standing`
  with two additions, and `gc::decide` ages both windows: a chat and a checkout
  are one attempt's leavings, and a box that reclaimed the work while keeping
  every conversation about it forever would have tidied half the mess. The
  clock starts when the task leaves the board — merged, closed upstream, marked
  done — and is stamped on `attempts.chat_archivable_at`.
- **What it will not touch.** A live *or blocked* attempt (both are open
  attempts, and the agent that stopped to ask at 02:00 is the worst chat to
  file away). A task in review — review delivery asks `chat_alive` about
  exactly this chat, so archiving one would break its own delivery loop,
  silently, for the tasks a human is still working on. The pinned
  orchestrator, which hears about every settle and is therefore never
  finished. And a chat with no board attempt: the sweep walks attempts, so a
  hand-made chat is never a candidate — those are the human's.
- **Archiving is not deleting.** The mutation is the same `set_chat_archived`
  the sidebar's own Archive writes, through a new `Runtime` verb, so every
  surface updates off the workspace-doc watch with nothing told. The
  transcript is intact, Settings → Archived unarchives, and the board
  un-archives a chat itself when a wrongly-settled attempt goes back to work
  (`rewatch_settled_attempts` — but only one it archived; a chat an operator
  filed away is theirs).
- **Per route**, unlike `retain_worktrees`: a shelf belongs to a space, routes
  are how work is pointed at spaces, and the route running a hundred throwaway
  fixes a week into a scratch space is not the route whose finished chats
  somebody re-reads.
- **`doctor` says what it costs.** A `chats` check reports how many board chats
  are still on their shelves, how many are on the clock, the window, and how
  many routes answer differently. Never red: keeping everything is a choice,
  and `off` is worded as one.
