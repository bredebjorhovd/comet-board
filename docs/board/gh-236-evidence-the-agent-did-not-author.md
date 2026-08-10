# Evidence the agent did not author — **done** (gh#236)

The second of the five children of gh#234. §gh#235 finished the claim
*contract*; this one is about what stands underneath a claim once it has been
made.

The design states the problem in one sentence:

> *"A summary written by the model that wrote the code inherits its blind spots
> — a misunderstanding is described fluently and confidently. So every claim
> carries evidence the agent did not produce."*

§gh#183 had already built two halves of that: the **diff** knows which files
moved, and the **run journal** knows which commands ran and how they exited.
It named the rest as follow-ups rather than half-building them — which tests
are new, whether the public surface moved, what happened to the schema, the
config, the dependencies. gh#236 is those follow-ups, and one screen row to put
them on.

`crate::effects` is the module. Nothing in it is parsed out of anything an
agent said. Its inputs are the unified diff of `base..HEAD`, the two trees on
either side of it, and the journal — all read by the board.

### The effects row

Five chips, in the order a reviewer asks the questions.

`Tests 41 → 47, all passing` is two facts and says both, because either alone
misleads. The **counts** come from `git grep` over each tree with the same
pattern list — Rust attributes, `func Test`, `def test_`, `it(`/`test(`,
`func test` — so a language the list misses is missed symmetrically and the
*pair* stays comparable even where the total is low. The **verdict** comes from
the journal: `evidence::runs_tests`, a subset of `VERIFICATION` spelled
identically to it so a command cannot be a test runner and not a check. Six new
tests nobody ran is not evidence, and a chip that said "all passing" about a
suite nobody executed would be the failure this screen exists to prevent — so
that one reads `never run`, in the working hue.

`Public API unchanged` is the chip with the sharpest failure mode, and it is
the one built to fail loudly. The rule is per language (`pub ` and not
`pub(crate)`, `export `, a module-level `def`/`class` without a leading
underscore, an exported Go identifier, `public`/`open`), and a changed file in
a language the board has **no** rule for makes the whole answer
`Surface::Unknown` rather than `Surface::Unchanged`. A file that could not hold
a public API at all — a lockfile, a changelog — is irrelevant to the question
rather than unknown, so an ordinary Rust branch with a README in it still gets
a real answer.

`Schema unchanged` and `No config keys added` are lists of spellings: SQL DDL
on any changed line (which is how this repo's own migrations move — they live
in `db.rs`), a schema-shaped path, and `key = value` in a config file that is
not a manifest. These two can miss quietly, and the module doc says so rather
than implying otherwise. They are pointers; the unclaimed set is what holds the
line.

`1 dependency added` is the one chip that is not neutral, and it is not a
heuristic either. The manifests are read on both sides — `git show base:path`
against `git show HEAD:path`, parsed as Cargo/`package.json`/pyproject/go.mod/
requirements — and the chip names what the new file lists and the old one did
not. A version bump is not a new dependency. A manifest that will not parse
makes the chip say **unknown**, never "none": that is the one thing the board
must not say about a file it failed to read.

### The chips under one claim

`4 new tests pass` counts test declarations the diff added *in the files that
claim's own anchors reached*, and takes its verb from the journal. `1 call
site, was 2` counts, in both trees, the lines naming a symbol anchor that do
not also declare it — two `git grep -h -w -F`s, run when somebody opens a
review rather than on every reconcile, because they need a tree to grep.

`no test covers this` is said whenever no test in this branch reaches anything
the claim is anchored to. It is not an accusation; plenty of correct work has
no test. It is the difference between a sentence and a checked sentence, and it
appears beside a call-site count rather than instead of one — "somebody calls
it" and "something checks it" are different news.

`ClaimMark` is the glyph, and it has three states because a claim has three:
`!` for one the diff refuses outright, `✓` for one something the agent did not
author stands behind, `·` for the middle — anchored, and corroborated by
nothing. A boolean here would have flattened exactly the distinction the
design asked for.

### Where it is derived, and where it is not

`SyncEngine::branch_facts` replaces `branch_changes` and reads the same three
`git diff`s it always did, now returning the effects beside the changed set.
`record_review_facts` writes both onto the attempt row — on the same condition,
so a branch that has not moved does not get two `git grep`s per tick, and
additionally on any row that has no effects at all, which is every attempt that
was live when this landed.

The snapshot matters for `gh#72`'s reason: gc reclaims the checkout, and a
review that kept the remainder and lost the effects would go from "the board
looked" to "the board never looked" purely because a sweep ran. Call-site
counts are the exception and are deliberately not stored — they need a tree, so
after collection they are simply absent rather than stale.

`Effects::read` is the field that keeps all of this honest across a restart.
`attempts.effects` is NULL on every row written before this existed, which
deserializes to `read: false`, whose chip row is one line — *no effects read
from this branch* — rather than five chips of reassurance nobody earned. That
is the ticket's exit condition, and it is what `Effects::default()` is designed
to render as.

### What the board does not do

It does not run the tests. The reconcile loop visits every live attempt, and a
suite per attempt per tick would cost more than the screen is worth — on this
box, a `target/` per worktree (§gh#186). What it does instead is read the exit
status of the runs that *did* happen, which the harness recorded without being
asked and which the agent did not write.

Nothing here is folded into `findings()`, and so nothing here can change the
verdict bar. A new dependency and an unrun suite are things to look at, not
things nobody accounted for; the one loud voice on this screen still belongs to
the unclaimed set (§gh#183). A screen where three blocks shout has nothing left
to shout with.
