# Live agents in the sidebar — **done** (gh#103)

*(Since gh#123 — §gh#126 — this group and §gh#117's draw as one **Active** section;
every rule below is unchanged, minus the header.)*
In herdr every working agent was a pane, so the pane list *was* the presence
list and presence cost nothing. Here a dispatched agent is a chat among chats:
three of them are three rows somewhere in a recency-sorted list, indistinguishable
from the session you opened yesterday, and tracking them meant the board pane or
nothing.

Both sidebars grew an **Agents** section between Spaces and the sessions. One
row per live attempt: the issue identifier as the title, the branch underneath,
and elapsed against the route's cap on the right. Pure presentation — everything
it draws was already streamed, and nothing here dispatches, settles or decides.

- **`comet_proto::view::board::agent_rows`** is the whole derivation, shared as
  the architecture rule requires: `WatchBoard` rows joined to the chat rows and
  the session watch. Membership is "`working` or `blocked` **and** has a chat
  id", which is why a row leaves on its own — settle, cancel and orphan all end
  the attempt, clearing `chat_id` and moving the row out of both states in the
  same frame. The chat stays findable under its space, as it always was.
- **The state is the session watch's, not the row's** (`AgentState`). The board
  is a sync cycle behind, and it calls a dead run and an agent asking a question
  both `blocked` — correctly, since both hold a chat and a slot, but they want
  different things from a person. The sidebar splits them: a spinner, a blocked
  badge, an errored glyph. Staleness-gated through `effective_indicator`, so a
  crashed backend cannot leave an eternal spinner; the row's own state is the
  fallback for a chat whose session mirror does not exist yet.
- **Blocked floats, with a count on the header** — the board's section-order
  rationale, and the same ranking `attention_rank` gives chat rows. Under it,
  longest-running first, which is stable because that order is start order.
- **`TaskRow.max_duration_secs`** is new on the wire. An elapsed counter says
  half of what it knows without the cap beside it ("1h50m" means one thing under
  two hours and another under six), and the routing config lives on the board's
  host — a laptop reading a relayed board has never seen it. Past the cap the
  counter turns and bolds: gh#70's clock is about to end that attempt, and the
  number is the reason.
- **The desktop's board subscription is now standing.** It was lazy — no RPC
  until the dock was first opened — and a presence list that only works after
  you have visited the board is not presence. `BoardPanel` is built with the
  shell and observed by it; the host sweep is unchanged and bounded, and it is
  what `comet-tui` has always done (its board stream has been standing since
  §board-view).
- **The TUI pays one wake-up a second** while a live agent row is on screen
  (`App::counting`), which `animating` does not cover: a *blocked* agent
  animates nothing, and its age would otherwise sit at whatever the last frame
  happened to catch. The row carries the start instant, not the age, so the
  draw reads the clock and nothing rebuilds.

Deliberately not here: acting on a row. Enter/click opens the chat and that is
all — retry, cancel and dispatch stay in the board pane, which is the deep view
and has the confirmations. A glance that can kill an agent is a glance nobody
trusts.
