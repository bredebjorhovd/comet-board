# The claim contract, its storage, and the unclaimed remainder — **done** (gh#183)

The backend half of gh#180, and the larger half. gh#180's argument in one line:
a summary written by the model that wrote the code inherits its blind spots, so
every claim must carry evidence the agent did not author — and **the interesting
set is the changes no claim accounts for**. None of that was renderable, because
nothing asked for claims and nothing stored them.

Three parts, in the order they had to land, plus the one that had to be scoped
rather than half-built.

### The format, asked for where the work starts

`assets/skills/comet-board/SKILL.md` is compiled into the binary and written
into every dispatched agent's slot on every dispatch (§gh#133), so it is the one
place a format can be *asked for* rather than hoped for. It now carries a
finishing step:

```
<what you did> :: <path> [<path>…]
```

`comet-board claim --task <id>` takes a block of those on stdin (or `--claim`
per line). The refusal is the contract: a line with no `::`, or nothing after
it, is rejected by name — `crate::claims::parse` — and nothing is recorded. An
unanchored sentence is prose, and prose is exactly what this replaces.

Three decisions inside the parser, each because the obvious alternative is
wrong:

- **The last `::` wins.** `Db::open now migrates :: crates/board/src/db.rs` is
  the sentence people actually write, and splitting on the first separator would
  make Rust paths unwritable.
- **A directory anchor accounts for what is under it.** `crates/board/src/` is
  the honest spelling of "I rewrote this module", and it is visibly coarser than
  naming the files.
- **A bare filename anchors nothing.** `db.rs` for
  `crates/board/src/db.rs` is refused a match — three crates here have a
  `db.rs`, and the generous reading would silently claim files nobody looked at,
  which is the exact failure the design is built around. The anchor comes back
  in `unmatched_anchors` instead, where a claim about work that did not happen
  is as visible as a change nobody claimed.

### Storage against the attempt

`attempts.claims` / `attempts.claims_at`, beside `pr_url` and the outcome, and
on `Attempt` itself. On the *attempt* because a retry is a different agent on a
different branch making different claims — and because the review happens after
the chat is archived (§gh#139), so the conversation that produced them is not
somewhere the board can go back and read.

`NULL` and `[]` are kept apart on purpose. An attempt that never answered the
contract claimed nothing and was never asked; an agent that submitted an empty
set said something. `claims_at` is the witness, and every surface keeps the two
distinct all the way to the print.

Submitting again **replaces** the set. Claims are an attempt's answer to one
question; an agent that submits twice is correcting itself, and appending would
leave a review reading a superseded claim beside the one that replaced it.

### The remainder, computed

`crate::claims::remainder` maps claims onto the branch diff and returns what
they do not reach. Two properties do the work:

- **The diff is read by the board, from git, never taken from the submission.**
  `SyncEngine::branch_changes` runs `git diff --numstat`/`--name-status` in the
  attempt's checkout against its own `base_sha` — the same base
  `attempt_has_commits` measures from, and for the same reason (AGE-19):
  anything else counts the operator's unpushed work as the agent's, and a
  *remainder* computed against the wrong base invents unclaimed files out of
  somebody else's commits. An attempt with no recorded base gets no guess; the
  review says so.
- **Renames are read with `--no-renames`.** To a reviewer a rename is two paths
  that both changed, and a claim naming only the old one has not accounted for
  the new name arriving.

The remainder is answered back at **submission time**, not only at review time.
`comet-board claim` prints the unclaimed set as its reply, so the agent learns
which of its own changes nothing it wrote covers while it is still the party
that can do something about it.

### Evidence: what the board already observes

Scoped to what needs no new instrumentation, which was the ticket's own
instruction. The run journal records every `ToolCall`/`ToolResult` pair already,
so `RunJournal::commands` reads back what the run executed and how it exited,
and `crate::evidence::gather` sums it: totals over every command, plus the
recognised verification commands deduplicated with their run and failure counts.

`is_check` is a prefix list matched per shell segment (`cd x && cargo test`,
`RUST_LOG=debug pytest`). It will miss an unusual runner, and that is the safe
direction: a missed check under-states the evidence, and the totals beside the
list keep the miss visible — *0 checks among 214 commands* is itself a finding,
and one the list cannot suppress.

**Named as follow-ups, not half-built**: which tests ran and which are new (a
harness would have to parse a runner's output); call sites that moved and schema
changes (a diff parser per language). Neither is in this ticket.

### Both facts are copied, not looked up

`record_review_facts` snapshots the branch diff and the command evidence onto
the attempt on every reconcile of a live attempt and once more inside `settle`,
for the reason `record_tokens` gives (§gh#151): gc reclaims the checkout
(§gh#72) and a journal can be compacted, while the attempt row survives for as
long as the board has a history to report on. A review taken purely at read time
would go blank exactly when it is most useful — a fortnight after the merge,
when somebody asks what that branch actually changed.

`review` reads the checkout live when it is there and falls back to the
snapshot, and says which. It never renders a missing diff as an empty one:
"nothing changed" and "the checkout was reclaimed" are opposite answers, so
`DiffSource::Unavailable` carries the reason in the words the CLI prints.

### The surface

`comet-board claim` and `comet-board review [--attempt N] [--json]`, over two
relay-forwardable RPCs (`SubmitClaims`, `ReadAttemptReview`) like every other
board verb: the attempt row, the checkout the diff comes from and the run
journal all live on the box, and the agent submitting is usually not sitting on
it. The text is parsed on the *host* rather than in the client, so the refusal is
the same refusal whichever client sent it.

`--json` prints brief, claims, evidence and remainder in one object — which is
what leaves gh#180 a rendering problem, as it should have been all along.
