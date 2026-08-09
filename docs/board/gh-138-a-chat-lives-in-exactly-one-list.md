# A chat lives in exactly one list — **done** (gh#138)

§gh#123's **Active** and gh#124's spaces tree answer different questions — "what
is alive" and "what lives here" — and both answered with a full session row.
Three agents working in one space therefore rendered twice inside one screen
height: full rows in Active, the same rows again under the expanded space. The
duplication was deliberate (status vs navigation) and overshoots exactly when
activity concentrates in one space, which is the common case.

- **Active owns a chat while its session is live; the space's shelf shows it
  when idle.** `comet_proto::view::spaces::space_shelf` is the split, over
  `active_placements` — the `(chat id, space id)` join Active's rows need,
  since an `AgentRow` names an issue and not a folder. Membership is
  §gh#123's list verbatim: nothing new decides who is alive, and the tree simply
  stops re-listing what Active already said.
- **Two seams keep the surfaces tied.** The space row keeps its aggregate dot
  (how urgent) and gains `running_label` — `· 3 running` (how many, and where
  they went); a space whose sessions are all up in Active discloses
  `shelf_note` — "3 running above · no idle sessions" — instead of a gap that
  would read as a bug. The count comes from the placements, not the tab order,
  so an archived-but-working chat is still admitted.
- **A repo slug is not a folder.** The same screenshot showed
  `bredebjorhovd/attn` twice in the local group: `~/dev/attn` and the board
  worktree at `~/.comet-native/worktrees/attn/board-gh-10-attn`, two real
  spaces on one machine that gh#118's repo-first naming calls the same thing.
  Not a `device_groups` sort bug — the grouping is total and both rows were
  correctly in the header-less local group. `space_titles` now makes names
  unique *within* a group by appending the shortest path tail that separates
  them (`· attn` / `· board-gh-10-attn`); across groups the device header
  already tells them apart, so gh#124's "named once" stands.
- **All three surfaces, one derivation.** Desktop derives Active once per
  sidebar frame and hands it to both sections; the TUI subtracts the same
  placements when it builds its nested `Row::Chat`s and carries `running` on
  `Row::Space`; the phone applies it to the home screen's Sessions list, where
  Active sits directly above (`SpaceRows.swift`, the Swift port). `SpaceView`
  is a different screen with no Active on it and stays complete.
