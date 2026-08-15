# Tokens: the number the engine was throwing away — **done** (gh#151)

§gh#143's page could say how long the work took and never what it spent, because
nothing persisted a token. `AgentEvent::Usage` was emitted by both harnesses
and dropped on the floor in `doc/parts.rs`, on purpose — token display is
excluded from *docs* by design, a poor fit for CRDTs. That exclusion was read
as "the board does not count tokens", which does not follow: the board has its
own SQLite store and its own history.

Two things had to be settled before anything could be added up, and both are
pinned by tests:

- **The cache fields were never parsed.** Claude's result frame carries
  `cache_creation_input_tokens` and `cache_read_input_tokens` beside
  `input_tokens`; the wire struct read the first two fields only. On any
  session past its first turn the cached half is most of what was read, so the
  old two numbers under-reported a run by an order of magnitude.
- **Codex reports a snapshot, and counts cache *inside* input.**
  `thread/tokenUsage/updated` re-fires through a turn with that turn's running
  total; the session loop already held it in `pending_usage` and flushed once
  at `turn/completed`, so what reaches the journal is one figure per turn —
  the event is per-turn for both harnesses, and summing over a journal is a
  sum over turns. Its `inputTokens` *includes* `cachedInputTokens`, the
  reverse of the Anthropic shape, so the normalizer subtracts. `TokenUsage`'s
  four buckets are disjoint by construction, which is what makes `total()` a
  plain sum rather than an argument.

- **Journal → attempt row, copied while the evidence exists.** The engine's run
  journal is the source (the settle authority already reads it, so a crash
  mid-attempt loses nothing) and the attempt row is the record, because the two
  have different lifetimes: §gh#144 archives a chat once nobody is coming back to
  it. `Runtime::run_tokens` sums a chat's journal — filtering lines by tag
  before parsing, since a long run's journal is mostly text deltas — and the
  reconcile copies it onto the row **before** any branch that can close the
  attempt, so an orphaned or capped run keeps what it had spent. Cancel and
  retry-replace read it themselves; they never pass through reconcile.
- **The model, because nothing else states it.** `DispatchSpec::model` is
  `None` on most attempts — the route named no override and the harness default
  ran — so a per-model breakdown keyed on it would be almost entirely
  "unknown". What the harness announces in its `SessionStarted` is the model
  that actually ran, and it is recorded beside the tokens.
- **Blank, never zero.** Five nullable columns with no default, written as a
  set. NULL means "this attempt reported nothing" — every row from before this
  existed, and any harness that meters nothing — and the page counts those out
  of its coverage rather than adding a zero to a total. The same rule §gh#143's
  `completion_rate` follows, and for the same reason: a zero reads as free
  work. Backfill is not possible and would not be honest if it were.
- **Coverage said out loud.** Every token card carries "62% of attempts
  reported usage (8 of 13)" as its aside. A total read without it is a total
  read wrong. `token_coverage` is `None` when nothing ran and `Some(0.0)` when
  attempts ran and none reported — those are different facts.
- **Counts first; pricing followed separately.** gh#182 added the dated API
  list-price table and gh#426 added exact mixed-model and agent attribution.
  Work absorbed by a Claude Max seat did not cost a per-token figure, so every
  money surface labels the result a list-price API estimate, not a bill.
