# Stats are a union, not a host choice (gh#461)

Comet now has two deliberately different stats reads:

- `BoardStats` is the compatible, deterministic view of one `board.db`. The
  existing per-board selectors and `comet-board stats [--json]` retain that
  meaning.
- `AggregateBoardStats` is an explicit, on-demand union. Desktop and iOS offer
  **All boards**, and `comet-board stats --all-boards [--json]` prints the same
  contract they decode.

The aggregate contains three layers rather than only a total. `stats` is the
merged `BoardStats` renderers already understand; `boards` carries every
contributing board's canonical id, host and individual stats; `hosts` records
every transport candidate as `answered`, `duplicate`, `noBoard`,
`unreachable`, or `unreadable`. `complete` is false for the last two states,
and every surface says that the visible totals include only answering boards.
An unreachable host is therefore never converted into an empty board.

### Identity and deduplication

Each database gets a random `board_id` in its existing `meta` table the first
time this version opens it. The id belongs to the store, not to a device route,
repository, or task. Two device candidates that reach the same store answer
with the same id and contribute once, while both transport paths remain in
`hosts` for audit. Two independent stores polling the same repository have
different ids, so all of their attempts remain in the union.

The id is the only new persisted data. It moves with `board.db`, which is the
property deduplication needs: moving a board to another host must not make its
history look like a new board. The aggregate, host failures, and merge inputs
are never persisted.

### Merge semantics

Each board applies the requested time window before answering. Additive facts
(attempts, outcomes, live work, tokens, time, daily/hour buckets, landing,
friction, sources, runtimes, spaces, accounts and context coverage) are summed.
Completion and token coverage are recomputed from their merged numerators and
denominators rather than averaged.

Two values need more source detail than `BoardStats` normally exposes. A
`BoardStatsSnapshot` therefore includes the window's sorted attempt durations,
so percentiles are recalculated over the union, and its unfolded breakdown
rows, so the aggregate ranks and folds only after equal labels have merged.
These inputs exist only for this request and are retained by neither side.

Pricing keeps the single-board honesty rules. Known list-price estimates add;
usage a board could not price (an unknown model or absent rates) remains
explicitly unpriced. Each snapshot also identifies configured payer ids as
shared and unnamed/default logins as board-local. Shared ids merge across
boards; board-local labels are qualified with `board_id` before account
attempts, tokens, breakdown rows, spend and plan declarations are combined.
Thus an identical plan for one known shared account is one subscription, while
two boxes' default subscriptions remain distinct. Conflicting declarations for
one shared id are omitted from the aggregate comparison while individual board
answers retain them for audit.

An explicitly disabled board answers `noBoard`. If the engine intended to host
a board but could not start its service or read `board.db`, it retains that
failure and the probe answers `unreadable`; the aggregate is therefore
incomplete rather than silently treating the host as zero activity.

### Ownership, freshness, and cost

The engine receiving `AggregateBoardStats` is only a collector. Every board
continues to own and calculate its own current SQLite snapshot. The collector
asks the synced set of engine-capable devices concurrently, gives each five
seconds, deduplicates by returned board id, and merges the answers in a stable
order. The result is request-time fresh per board, but intentionally not a
distributed transaction: the host audit rows state exactly which snapshots
were available for that read.

This is fan-out read traffic only when a person opens or refreshes Stats, or
runs the explicit CLI flag. A dark fleet costs at most one five-second wall
clock budget because probes run concurrently. There is no timer, background
sync, edge polling, Durable Object write, cloud stats cache, or continuous
replication. A shared persistent store would add ownership, retention,
consistency and Cloudflare-cost questions without improving the truth of an
on-demand operational view, so this implementation adds none.
