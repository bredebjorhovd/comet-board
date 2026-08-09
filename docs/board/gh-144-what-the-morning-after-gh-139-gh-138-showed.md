# What the morning after §gh#139/§gh#138 showed — **done** (gh#144)

The operator looked at the same sidebar the next morning (2026-08-08) and the
two fixes read as no fix at all. Neither was wrong; both stopped one step short
of the screen.

- **A disambiguator at the end of a name the sidebar elides from the right is
  the first thing cut.** §gh#138's `space_titles` correctly named the two attn
  checkouts `· attn` and `· board-gh-10-attn`, and both rows still drew
  `bredebjorhovd/attn…`: the pane is narrower than the slug alone. The tail is
  now a field, not a suffix — `SpaceTitle { base, qualifier }`, with
  `line()` for the surfaces that have room (the TUI, the drag ghost). The
  desktop row gives the qualifier its own `flex_none` width (capped at
  `SPACE_QUALIFIER_MAX`, so the chevron stays put) and lets the *base* shrink
  first. The half that differs is the half that survives.
- **A week is the checkout's clock, not the chat's.** §gh#139 gave both the same
  window on the theory that they are one attempt's leavings. They are not read
  the same way: a checkout is evidence you might go back for, a chat is a row
  you are *shown*, and thirteen finished rows from one night's work buried the
  live ones — "having the issues alive and not collected is kinda worthless
  really". `[defaults] archive_chats` is **`on-settle`**: no window. The guards
  are what protect an unfinished chat, and they are all in `chat_standing`
  already — live attempt, blocked attempt, open pull request, issue still open,
  the pinned orchestrator, a chat nobody dispatched. When none of them hold, the
  task has merged or closed and the row has nothing left to say. A duration
  still works for a space that wants a grace period; a bare `0` is now an error,
  because it reads as "no window" here and "keep forever" for a checkout, and
  guessing between opposites is worse than asking.
- **A row you have to look up is not a row.** A dispatched chat was named for
  its identifier alone, so a shelf read `gh#10 gh#25 gh#26 gh#11 gh#13`.
  `DispatchSpec` now carries the task's `title` and `chat_title()` composes
  `gh#25 · D1 Prototype v1: the Today window (static)` — identifier first,
  because it is short, it is what the board rows and the branch sub-line say,
  and it is therefore the half that has to survive a narrow pane. The title is
  clipped at 60 chars on a word boundary; an empty one leaves the bare
  identifier rather than a dangling separator.
- **The kill switch was behind a row that does not exist.** §gh#125 put unpinning
  on the pinned chat's own row — "whoever wants the notices to stop reaches for
  the session they pinned" — and gh#122's slot is not that row. Exit the
  orchestrator's session and its chat leaves Active; if its space shelf never
  listed it, the slot above Spaces is the only row it has, and that row had
  `on_click` and nothing else. The operator could reopen the thread and not
  unpin it from either app. The slot now carries the same context menu a chat
  row does, on both surfaces (`render_orchestrator_slot`, and `Row::Orchestrator`
  in the TUI's `open_context_menu`, which fell through to `_ => return`). The
  CLI escape hatch was always there and nobody should need it:
  `comet-board routes defaults orchestrator_chat --unset`.
- **Screenshots in a PR body die twice.** Not a board bug, but the board's
  agents keep writing it: an attempt asked for screenshots in its PR
  description and reached for
  `raw.githubusercontent.com/<owner>/<repo>/<branch>/…`, which is unreadable
  without a token on a private repo *and* names a branch that merge deletes.
  Both failures are silent. `docs/agent-conventions.md` (and the shipped
  skill's **Screenshots in a PR description** rule) now says: commit the images
  and reference them with a relative path from a markdown file in the repo.
