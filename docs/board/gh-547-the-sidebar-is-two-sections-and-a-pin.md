# The sidebar reads as two sections and a pin — **done** (gh#547)

Brede, 2026-08-20: *"the UX of the board … feels a bit strange to read wrt
hierarchy. I know I mentioned this before."* He had. gh#171 was the visual
system pass — greys, a status ramp, radii, type sizes. This is the level above
it: not which grey, but **what contains what**, and which of these rows is the
same task twice. Four decisions, made once and written down:

### 1. Needs you stays a section — and says it is a projection

The inbox is not a fourth place things live; it is a view over the places
below. Nothing about membership moves a chat: a need answered and the row is
gone, while the row it pointed at stays exactly where it lives. That was true
in the derivation all along (`needs_you` subtracts nothing from anywhere);
what was missing was the reading, so the module contract now states it and the
composition backs it up — the inbox is the only section that can be empty *in
words* (`Nothing needs you`, kept exactly as gh#122 reasoned), which is what a
filter looks like and a folder never does.

### 2. Two levels, typed

The sidebar has two levels: **section** (`Needs you`, `Spaces`) and **group**
(`@ device`, `Unfiled`). Every header used to render at caption/500/subtle —
a top-level section and a group inside one were typographically identical, so
depth rode indentation alone, and indentation is the first casualty of a
narrow sidebar or a truncated name. Sections now carry 600, groups stay
regular — which is not a new decision but an old drift corrected: the canvas
claims C1.1/C2.2 said 11px/**600** from the start. No new token, no size
change; #171 owns the scale and this pass only assigns the levels to it.

### 3. The pin comes out of Spaces

The pin used to be the first child of the selected space's disclosure,
following canvas claim C2.3 as drawn: it belongs to the space whose board it
serves. The reasoning was fine and the nesting was still wrong — it is one
address for stray notices about *every* space, and housing it inside one
disclosure meant it vanished whenever another space was selected. A pinned
conversation that hides is a hidden one. It now draws as its own slot between
Needs you and Spaces, with the section hairline under it:

- The phone drew the slot there from the start (`HomeView.swift`); the
  derivation's own contract (`view::needs::fallback_slot`) always said "above
  Spaces"; the desktop caught up.
- Its chat is held out of every disclosure — live and idle — so the pin
  appears once per sidebar, wherever its chat's space happens to be. The old
  scoping (held only in the selected space, ◆-marked rows elsewhere) existed
  to patch housing a global fixture inside one branch.
- The forced-open machinery died with the placement:
  `space_disclosure_forced_open` existed so a collapsed disclosure could not
  hide the slot; with no slot in any disclosure there is nothing to force.
- Selection got *better*: the slot now fills when its chat is the selected
  one, which it could not do while the parent space owned the fill.

Naming follows gh#348, which landed while this pass was in review and
dissolved the orchestrator *role* this ticket had inherited: the thing is an
address for stray notices — `[defaults] fallback_chat` — and nothing more.
That sharpened the placement decision rather than reversing it: an address
that is nobody's chair has even less business living inside one space's
branch. `window.md` C2.3/C2.8/C2.9 are amended under a deviations note, the
same way D6's size deviation is recorded.

### 4. Loose Active joined Spaces, and the board pane says what it is

The section below the tree kept gh#123's name — `Active` — long after gh#258
split the live list into disclosures. A header promising "everything alive"
over only the rows no space claimed was a promise the rows did not keep; and
the issue's point stands: a live run with no space is the same kind of thing
as a live run with one. The group is now the tail of the Spaces section under
`UNFILED_TITLE` (`comet_proto::view::spaces`, tested), in the quiet
group-header voice, present only when such rows exist. The sidebar reads as
exactly two sections and a pin.

That leaves the pane. Six state sections and a tree are two organisations of
one queue, and pretending they agree is how the strangeness started; instead
each says what it is. The sidebar says "what wants me" first and "where things
live" second. The pane — openly the all-rows view — gained one phrase in its
header: when rows want a human it says "**N need you**", in the inbox's own
words, so the fact travels between the two organisations without a third
vocabulary. It is a fact, not a control: the inbox lives in the sidebar,
always on screen.

### Kept, deliberately

Density (no row grew, no header got taller than it was), the words-only empty
state, the glyph families a blocked attempt shares across inbox, tree and pane
(gh#171's ramp did that work; nothing here re-does it), and out of scope by
the ticket: colour, radii, type scale.
