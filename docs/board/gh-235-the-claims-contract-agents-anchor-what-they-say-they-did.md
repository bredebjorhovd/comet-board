# The claims contract: an anchor is a path or a symbol, and a finished attempt is read — **done** (gh#235)

The first of the five children of gh#234. §gh#183 built the contract, its
storage and the remainder; the review screen (§gh#180) renders them. What
gh#235 is about is the two ways that contract was still narrower than the
design it exists to serve, and one way it could still lose an answer.

The design's own reference claims are the specification:

> *"A live chat's row is drawn by Active and never by the spaces tree."* →
> `shell/spaces.rs`, `view/spaces.rs`
> *"Both surfaces read one derivation per frame, so they cannot disagree."* →
> `active_placements`
> *"An expanded space says so when Active holds every one of its rows."* →
> `shelf_note`

Two of those three anchor a **symbol**, not a file. Under §gh#183 they parsed —
and then matched nothing, because every anchor was a path and
`active_placements` is not one. The reference claims of the design were, in
their own product, three claims of which two came back unsupported.

### An anchor is a path or a symbol, told from its spelling

`crate::claims::anchor_kind`. A `/` settles it; failing that, a short
alphanumeric run after a final `.` makes it a path (`db.rs`, `Cargo.toml`);
everything else is a symbol. No sigil and no flag, because the format an agent
writes at 3am is the format it half-remembers, and a declaration of which kind
it meant is a field that gets it wrong in a way nothing can detect. The
spelling is already unambiguous to a reader — that is what makes it a good
rule.

`Claim` grows `symbols` beside `files` rather than becoming a list of a tagged
enum. Every claim already written into `attempts.claims` is a `{text, files}`
object, and a `#[serde(default)]` empty `symbols` reads those rows back as
exactly what they were: path-anchored claims, no migration, no guess about
what somebody meant a year ago.

#### What a symbol is matched against

`ChangedFile::symbols` — the identifiers the file's own diff **added or removed
a line naming**, read off `git diff -U0` by `attach_symbols`. `-U0` is the
whole of it: a symbol three lines above the edit is context the agent never
touched, and letting context anchor a claim is the generous reading this module
refuses everywhere else.

Three properties, each for §gh#183's reason rather than a new one:

- **Read by the board, from git.** A third read of the same range as the
  numstat and the name-status, against the attempt's own `base_sha`. An agent
  cannot widen its own denominator, and it cannot decide what its own symbols
  are either.
- **Lexical, and unapologetic about it.** No language parser: `is_check`'s
  prefix list will miss an unusual runner, and this will call a string in a
  comment an identifier. Both fail toward *under*-stating what is known, and
  the review prints every file an anchor reached, so an anchor that swept wider
  than its author meant is visible in the same place its matches are.
- **Bounded, and the bound loses in the safe direction.** 200 symbols per file,
  4 MiB of diff. A symbol that fell off the end costs its anchor a match, and
  an unmatched anchor is the loud outcome. Neither bound can invent one.

`names_symbol` is exact and case-sensitive, for `accounts_for`'s reason: `note`
must not answer for `shelf_note` any more than `db.rs` answers for
`crates/board/src/db.rs`.

What it deliberately does not claim to be is a *definition*. A rename touches
every call site, and a review that listed only the file holding the `fn` would
be hiding the other eleven — so a symbol accounts for every changed file whose
diff names it, and the claim's matched list says which those were.

### The block, read off a finished attempt

`comet-board claim` is the better path and stays the one the skill asks for
first: it answers with the remainder, to the one party still able to do
something about it. But it is a verb, and an agent that finishes without
running it has still written down what it did — in the fenced ` ```claims `
block the skill now asks for. Losing that is a worse outcome than reading it
late.

`claims::find_block` takes the **last** such block, on `set_attempt_claims`'s
rule: an agent that wrote it twice is correcting itself, and reading the
superseded one would be reviewing a draft. Fences of any other kind are skipped
whole, so a `claims` block quoted inside a shell snippet is not mistaken for
one. An unterminated fence runs to the end of the text — a message truncated
mid-block still carries claims, and refusing them over a missing ``` would
teach nobody anything.

The text comes from `Runtime::run_message` → `RunJournal::final_text`: the last
64 KiB of the chat's `TextDelta`s and `Done` results, off the journal that was
being written anyway. **Read for the fence and nothing else.** The argument the
whole review contract rests on is that prose written by the model that wrote
the code is not evidence, and this method does not get to be the hole in it —
it is never rendered, never summarised, and `claims::harvest` is its only
caller.

`SyncEngine::harvest_claims` runs once, first thing inside `settle`, ahead of
the snapshot the claims are checked against. It never overwrites: an attempt
that already answered has given its considered answer, and this is a scrape.

### Three outcomes, and only one of them is loud

The exit condition of the ticket, and the reason `Harvest` has three variants
where two would have compiled:

- **No block.** Nothing is recorded and nothing changes. The attempt settles,
  the PR opens, and the review says the contract went unanswered — exactly the
  behaviour of every attempt before any of this existed. Nothing in the harvest
  can fail a settle: no runtime, no journal, no chat id and an unreadable
  journal all return quietly, because a claims feature that can stand between a
  finished run and its PR is worse than no claims feature.
- **A block that parses.** Recorded against the attempt, and logged as having
  come from the closing message rather than the verb.
- **A block that does not parse.** `attempts.claims_error`, and from there
  `FindingKind::MalformedClaims` — `Tone::Alarm`, ahead of and instead of
  `NeverClaimed`, printed whole by both the CLI and the review screen because
  the refusal names the offending line.

That last one is the distinction worth having a column for. `NeverClaimed` is
`Tone::Unknown` — an absence of evidence, and a quiet screen is honest about
it. A refused block is not an absence: the agent *did* describe its work, the
description exists, and nobody can check it. Reported, never dropped, and
louder than silence.

`set_attempt_claims` clears the column, for the reason it replaces rather than
appends: a set that parsed supersedes the block that did not, and a review
showing both would be showing a draft beside its correction.

### The format is asked for where the work starts

`assets/skills/comet-board/SKILL.md`, compiled into the binary and written into
every dispatched agent's slot on every dispatch (§gh#133). The finishing
section now states both kinds of anchor with the design's own examples, and
asks for the block by name — including what happens if it is absent (nothing)
and what happens if it is wrong (worse to read than having claimed nothing).
That is the sentence gh#235 exists to be able to write: enforced at dispatch,
not hoped for at review.
