# A collapsed space should say where it is — **done** (gh#229)

Part of gh#171, and the smallest of its steps by diff and the loudest by
row-count: every row in the sidebar's tree changed shape. Most of the delta the
audit found against `Comet Window.dc.html` turned out to be empty state — `· 3
running`, per-chat branches and attention-coloured dots all shipped already and
simply had nothing to say on a quiet board. What was genuinely missing was one
fact, said in three places: **where a thing is checked out.**

### A space knows its branch now, and it is a synced fact

`Space` gains `branch`, stamped by the owning device beside `gitDetected` and
`checkoutId` (`SpacesSync`), cleared when the folder stops being a work tree.
It is a *stamp*, not an RPC: a phone that will never see that disk reads the
branch out of the workspace doc, and a laptop with no board host reads it too.
The alternative — asking `ListRefs` per space per frame — is five git
subprocesses per row on a surface that redraws on every keystroke.

The watchers had to grow one: `SpacesSync` watched the space folder
non-recursively for `.git` appearing or vanishing, and a branch switch happens
one level further in, at `.git/HEAD`. There is now a second non-recursive watch
on `.git`, filtered to the name `HEAD` — filtered because `.git/index` is
rewritten by every `git status` a build script runs, and an unfiltered watch
would spend three git subprocesses per rebuild. A folder that becomes a repo
after the fact drops its entry so the next reconcile installs the watch it
could not have installed before.

### A ticket row is an identifier and a branch, not a sentence

`DispatchSpec::chat_title` names a dispatched chat `gh#144 · <the issue's
title>` (gh#139), which is right for eliding — the recognisable half is on the
left, where truncation never reaches — and wrong for scanning: in a shelf of
them the identifier reads as the first two words of a sentence you have to
finish before you know which row you are on.

So a ticket-backed row is now **one line**: status dot, the identifier as a
mono chip, then the branch as the flexible element, then the age. No prose
title at all. The chip is found by shape; the branch is what you would have
gone looking for next, and it takes the place the issue's title used to have. A
dispatched chat with no branch of its own falls back to its title there — a
chip and a clock with nothing between them is not a row.

A chat a human started keeps its **title**, and gets the branch on a second
line behind the harness's own mark at 10px. The two shapes divide the second
line honestly: the row with a name worth reading keeps its name and spends the
line below on where and by what; the row whose name is an identifier says
everything on one.

`chat_ticket` (in `comet_proto::view`, tested) is deliberately conservative
about what counts: `gh#144`, `gh!220`, `LIN-142`, `gh:owner/repo#87` — and a
chat somebody called `notes · draft` keeps its whole title, because inventing a
chip for it would be inventing provenance. **Chips are for chat rows only**:
the Needs-you row's identifier stays plain text, since that row's job is to
read as a sentence in words.

### The space row, left to right

Glyph, name, branch, spacer, `N running` behind a dot in the colour of the most
urgent chat under it, chevron.

The dot moved. It used to lead the row and be permanently present — faint grey
when nothing was live — which spent the row's first pixels on a fact most rows
do not have, and made the reader check a colour to find out there was nothing
to check. It now appears only with the count it belongs to, at the right edge,
where both halves arrive and leave together; the spacer absorbs the change, so
the name never moves. (This reverses the gh#124-era "the dot leads so it cannot
jitter" note: what jittered then was a dot at the right edge with nothing
holding the column open. There is a spacer now.)

The glyph stopped keying on the GitHub slug alone. `is_repo_space` reads the
owner-stamped `git_detected` first and falls back to the slug, so `scratch` —
a plain directory — is a folder, and a checkout is a repo whether or not a
board host was reachable to name it. Keyed on the slug, a laptop that hosts no
board drew every one of its repos as a folder.

### Children hang off a rail

The disclosed rows sit inside a hairline guide rail (`margin-left: 9px;
padding-left: 14px; border-left: 1px`), and the orchestrator's pinned slot is
separated from everything below it by a hairline of its own. The slot is a chat
row like any other and it is not one of the chats: it is pinned, it outlives
every attempt, and it is the only row up there you are expected to talk *to*
rather than check on. Order said that faintly; the rule says it without your
having to have been told.

### What the other two surfaces took

The TUI takes the branch on its space row (after the name, before the count,
capped at 12 columns) and nothing else: a chip is a shape, and a terminal cell
grid has one shape. The chat rows there keep `gh#144 · title`, which in a
monospace column is exactly the aligned prefix the desktop cannot have.

iOS is untouched by this ticket — `Space.branch` reaches the phone through the
doc for free, and drawing it belongs with the rest of the phone's pass at this
design (gh#181).
