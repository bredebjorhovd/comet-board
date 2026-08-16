# Checks and conflict repair belong beside claims and effects — **designed** (gh#423)

The Review screen already answers the hard review question: what was asked,
what the agent claims, what the board observed, and what changed without a
claim. It still sends a reviewer to GitHub for the operational half of the
decision — checks, conflicts, draft state, and whether an accepted head can
land. This design brings that half back as a repository-state projection and
one action derived from it. It does not change the claim/effect reading and it
does not introduce another merge implementation.

This is a clean-room Comet design. The Zuse links on gh#423 informed the issue's
product question only. No Zuse source, UI, strings, schema, assets, or tests are
reused here; the data flow below is derived from Comet's existing sync,
`AttemptReview`, stack, verdict, runtime, and retention seams.

### The decision

`AttemptReview` gains a `repository` projection for its current pull-request
head. The projection owns the rollup, individual check contexts, draft and
merge state, current-head acceptance, freshness, and a single
`RepositoryAction`. Clients render that answer; desktop and iOS do not each
invent a state machine.

The repository band sits after the existing Stack band and before Asked for.
Its one-line summary remains pinned with the header and verdict; its individual
checks scroll with the review body. Failed contexts sort before pending ones,
then successful ones, with stable name ordering inside each group. Every row
has the check name, provider/workflow when known, result, duration, and its
GitHub link. Successful contexts may be collapsed initially, but every context
the projection contains remains reachable.

The verdict bar keeps the three review verdict types exactly as it has them,
but their availability is a core-derived fact on the Review response. A
`changes-below` row disables Comment, Approve, and Request changes together and
names the lower pull request that must settle first: the diff is about to be
replayed and cannot be reviewed yet. A closed or merged pull request disables
them too. Clients show the disabled controls and reason rather than hiding the
review boundary or attempting a verdict GitHub and the existing verdict path
will refuse.

The repository action occupies the existing quiet `Merge…` position and
changes with repository state. A repair is not a verdict, an approval, or
permission to merge. It starts another turn in the authoring chat and nothing
else.

### One head, one observation

The new wire shape is deliberately about an immutable head:

```text
RepositoryProjection
  observed_at
  head_sha
  base_ref
  base_sha
  layer_state               OPEN | CLOSED
  layer_merged_at           timestamp | null
  task_pr_open              existing aggregate: any task layer is open
  task_pr_merged            existing aggregate: the whole task stack landed
  draft
  mergeable                 MERGEABLE | CONFLICTING | UNKNOWN
  merge_state_status        GitHub's value, retained verbatim
  github_review_decision    APPROVED | CHANGES_REQUESTED | REVIEW_REQUIRED | null
  acceptance                accepted | changes_requested | needs_review | unknown
  review_state_truncated
  checks                    passing | failing | pending | none | unknown
  contexts_truncated
  stack_gate                standalone | complete | partial
  visible_layer_count
  github_stack_size
  topology_fingerprint
  verdict_controls          enabled | disabled_changes_below | disabled_closed
  contexts[]
  stale_reason
  action
```

Each context has:

```text
RepositoryCheck
  identity                  check_run:<database_id> | status:<graphql_id>
  kind                      check_run | status_context
  name
  provider                  check-suite app slug or "commit status"
  status
  conclusion
  details_url
  description
  started_at
  completed_at
  github_actions_job_id     only for a GitHub Actions check run
```

The projection is valid only when its `head_sha` equals the pull request's
current `head.sha`. A normal pull-list poll that sees a new head immediately
invalidates the old projection, including its approval, before any slower
repository query runs. Stale data may still be shown with its observation time
and reason, but stale data never derives `Mark ready`, `Repair`, or `Merge`.

The task record keeps its existing aggregate `pr_open` and `pr_merged`
lifecycle keys and grows `pr_node_id`, `pr_head_sha`, `pr_base_sha`,
`pr_merged_at`, `pr_draft`, `pr_repository_json`,
`pr_repository_observed_at`, and `pr_repository_next_probe_at`. The
variable-length contexts stay in the versioned JSON projection; the identity,
lifecycle, and head fields stay flat because the sync, mutations, and stack
join need them without decoding a UI payload. Data written before these fields
existed reads as an unknown projection, never as a clean one.

`AttemptReview.repository` carries the full current-layer projection. A
`StackLayer` carries only its `RepositoryGate` summary — lifecycle, head,
acceptance, checks, draft, mergeability, freshness, and blocker — so a review
can explain each layer without duplicating every sibling's check list on the
wire. That summary also persists the layer's pull-request node id, number,
head/base refs and SHAs, and stack position: the direct refresh and merge
preflight must address every gate-bearing layer without rediscovering it from a
truncated list. `layer_state` and `layer_merged_at` belong to the particular
pull request; they never derive the task row's `done`, retention, or collection
state.
`task_pr_open` and `task_pr_merged` are copied from the existing aggregate task
fields: for a stack, the first is true while any layer is open and the second
becomes true only after the whole stack has landed.

`stack_gate` is derived from the existing `RowStack`: standalone when
there is no stack, complete only when `size` is known and the visible layers
contain exactly one of every position `1..=size` including the current pull
request, and partial otherwise. `layers.len() < size`, a missing/duplicate
position, or an absent size is therefore action-visible uncertainty, not an
implicitly healthy stack.

`topology_fingerprint` is SHA-256 over a canonical encoding of repository,
stack number/size/base (or `standalone`), and the landing slice from bottom
through the current layer. Each ordered entry includes position, pull-request
node id and number, head SHA, base ref and SHA, `layer_state`, and
`layer_merged_at`. It changes when a lower head or base moves even if the
current head does not. The fingerprint is an optimistic-lock token for the
reviewer's merge confirmation, not a substitute for refreshing the gates.

### Exactly what GitHub is asked

No new timer is added.

The existing board cycle is `[sync] interval` from `routing.toml`, 30 seconds by
default and clamped to at least five seconds. Its existing
`GET /repos/{owner}/{repo}/pulls?state=all&per_page=100` call continues to run
once per configured repository per cycle. That unpaginated first page remains
the discovery/import source and an immediate invalidation fast path; it is not
a completeness proof for a persisted pull request. An old open pull request may
close after newer activity pushes it beyond item 100. The parser retains these
fields from each returned item:

- `node_id`, `number`, `html_url`, `state`, `draft`, `merged_at`, and
  `updated_at`;
- `head.ref`, `head.sha`, and `head.repo.full_name`;
- `base.ref` and `base.sha`;
- the existing preview `stack` object.

Repository state uses one GraphQL query per batch of at most 50 pull-request
node ids, grouped by repository and issued serially. It requests exactly:

```graphql
nodes(ids: $ids) {
  ... on PullRequest {
    id number url state isDraft mergedAt updatedAt
    headRefOid baseRefName baseRefOid
    mergeable mergeStateStatus reviewDecision
    statusCheckRollup {
      state
      contexts(first: 100) {
        pageInfo { hasNextPage }
        nodes {
          __typename
          ... on CheckRun {
            databaseId name status conclusion detailsUrl
            startedAt completedAt
            checkSuite { app { slug } }
          }
          ... on StatusContext {
            id context state targetUrl description createdAt
          }
        }
      }
    }
  }
}
```

The board's GitHub provider must choose the installation token using the
repository before posting `/graphql`; a cross-repository query cannot rely on
the current REST client's path-to-repository inference.

The refresh candidate set comes from persisted identity, not only from that
list: every task or visible stack layer still stored as open contributes its
`pr_node_id` to the repository queue even when the first page omitted it. A
GraphQL query is due for that identity when any of these is true:

- it has no projection;
- the REST list observed a different head or base;
- its last projection contains a pending context;
- its `observed_at` is at least the existing 120-second `FULL_SWEEP_SECS`
  cadence old.

At most 200 node identities per repository are refreshed in one board cycle:
four serial batches of 50. At least one batch is reserved for the oldest
overdue persisted identities; new/missing projections, list-observed head/base
changes, and pending checks may consume the other three before unused capacity
returns to the overdue queue. Within successful GitHub cycles, continuous
pending work therefore cannot starve lifecycle proof. Overdue identities sort
by `pr_repository_next_probe_at` then node id, and advance that timestamp only
after a successful response. This gives `N` persisted open identities a
worst-case lifecycle observation bound of the 120-second full-sweep interval
plus `ceil(N / 50)` ordinary board cycles; in the quiescent case all four
batches serve the overdue queue.

A legacy persisted pull request with no node id spends one targeted
`GET /repos/{owner}/{repo}/pulls/{number}` from a separate cap of ten legacy
identities per repository cycle, oldest first. The response stores its node id
and lifecycle, after which it joins the batched path; `L` legacy rows are thus
visited within `ceil(L / 10)` successful cycles. A missing/inaccessible node is
unknown and action-ineligible, never assumed closed. Neither path adds a per-PR
timer or an unbounded page loop.

A list item or targeted response with `state = closed` immediately replaces
that layer's projection and removes it from future open-identity refreshes.
Non-null `merged_at` produces a terminal merged layer; null `merged_at`
produces terminal `closed_unmerged`, sets that layer's action to
`Open closed PR`, disables its verdict controls, and explains that the branch
was closed without landing. Both layer transitions clear every cached mutation
for that pull request; historical checks may remain visible with their
observation time, but cannot derive Repair, Mark ready, or Merge. A GraphQL
response racing a list item is accepted only if `state`, `mergedAt`, head, and
base still match the newest lifecycle observation.

Layer terminal is not task terminal. A merged bottom with an upper layer still
open leaves aggregate `task.pr_open = true` and `task.pr_merged = false`; Review
stays live. Complete topology opens the lowest remaining open layer; partial
topology opens the stack on GitHub. Only the existing aggregate
`task.pr_merged` can hand the task to `done` and retention.

Thus a running build updates on the ordinary board clock, while a stable,
unchanged pull request costs only its slot in a batched response every existing
full sweep. There is no per-PR poller, no task spawned by opening Review, and no
background log fetch. Fifty ids per query, 200 refreshed identities per
repository cycle, and 100 contexts per id bound a cycle. `hasNextPage` sets
`contexts_truncated`; Comet shows the 100 contexts and a link to the rest, but
cannot call the head merge-ready.

The due decision is made from each projection's persisted observation time; it
does not re-read or advance `meta::LAST_FULL_SWEEP`. That existing key is
currently advanced by Linear's sweep before GitHub can consult it on a
mixed-source board, and coupling repository checks to it would make their
cadence depend on which source happened to run first. This keeps the 120-second
wall cadence while leaving one source unable to consume another's turn.

A failed batch preserves the last projection with `stale_reason` and marks the
GitHub source unhealthy through the existing metadata. It does not blank the
Review screen. Rate-limit replies obey `Retry-After`/reset on the normal sync
loop; they never start a retry loop of their own.

The existing review-feedback read gains `commit_id`, `user.id`, and
`user.login` from pull-request reviews. It rides the same `updated_at` trigger;
only a changed pull request can spend the bounded continuation pages described
below. `reviewDecision` is displayed as GitHub's branch-policy answer; Comet's
authority is the head-scoped standing verdict below.

### Acceptance belongs to the reviewed head

Today `Delivered::changes_requested = None` means both “approved” and “nobody
approved,” and its single last verdict lets one reviewer accidentally erase
another reviewer's objection. Neither can drive a merge action. `Delivered`
therefore gains a collection keyed by review, not one standing record:

```text
Delivered
  standing_reviews_schema_version   defaults to 0; current is 1
  standing_reviews_complete         defaults to false
  standing_reviews_updated_at       pull updated_at reconciled completely
  standing_reviews[]

StandingReview
  key                       github:<review_id> | comet:<submission_fingerprint>
  sequence                  monotonic PR-local id used by stack fan-out
  github_review_id          null until/unless GitHub accepts it as a review
  reviewer_key              GitHub user id (login only if absent), else verified Comet identity
  kind                      approved | changes_requested | unknown
  disposition               active | dismissed
  head_sha
  submitted_at
  source                    comet | github
```

A GitHub review read adds `commit_id`, `user.id`, and `user.login` and upserts
every decisive or dismissed review by its immutable review id *before* the
new-message watermark is applied. The review endpoint is paged at 100 records,
to a hard maximum of ten pages, only when the existing pull `updated_at` gate
says feedback moved. Once that timestamp differs, the board persists
`standing_reviews_complete = false` before the read; a crash cannot leave the
old collection labeled current. More pages set `review_state_truncated`; that
projection cannot derive Merge. A rewritten `DISMISSED` response marks that
exact review dismissed while retaining its previous kind when known. Seeing a
dismissal for the first time may leave `kind = unknown`; the tombstone still
has the review's identity, reviewer, head, and ordering.

A Comet verdict request carries the `expectedHeadSha` the reviewer actually
saw. Before recording, delivering, or posting anything, the board performs a
targeted pull refresh and compare-and-refuses unless that value equals the
current head. The refusal persists the refreshed projection and leaves no
submission, standing review, chat message, or GitHub review behind. Only then
does the verdict record that same expected SHA and the transport's verified
reviewer identity. The `[users]` mapping resolves that identity to GitHub's
numeric user id where possible; a normalized login is the legacy fallback, and
the authenticated Comet subject is the fallback for an unposted verdict. When
GitHub returns a review id, that id aliases the local submission rather than
adding a second decision. A verdict projected only as a comment or left
unposted remains a Comet decision, consistent with the existing rule that the
board's verdict stands even when GitHub refuses its copy. Comments do not enter
this collection.

Aggregation is deterministic and per reviewer:

1. order that reviewer's records by `submitted_at`, then immutable key;
2. take the newest record, even when it is dismissed;
3. an active approval or change request is that reviewer's standing decision;
   a dismissed newest record contributes no decision and does not resurrect an
   older review;
4. an approval changes only that reviewer's decision. It never clears another
   reviewer's change request.

`acceptance` is `changes_requested` while any reviewer has a standing change
request. A change request remains standing across pushes until that reviewer
replaces it or its exact review is dismissed. With no standing change request,
`acceptance` is `accepted` only when at least one standing approval names the
current head; approvals of older heads do not count. Otherwise it is
`needs_review`. Thus reviewer A's approval cannot erase reviewer B's request,
and a push invalidates every old-head approval even in a repository configured
not to dismiss stale GitHub approvals.

The existing `pr_changes_requested` column remains the cheap stack blocker. It
is recomputed from the collection as the newest active change request's
numeric `sequence` (or null), rather than mutated by whichever verdict arrived
last.

The schema marker makes migration distinguishable from a valid empty review
set. On load, an absent/older `standing_reviews_schema_version` or false
`standing_reviews_complete` sets `acceptance = unknown` and bypasses
`plan_delivery`'s unchanged-`updated_at` return. That return is legal only when
the schema is current, `standing_reviews_complete` is true, and
`standing_reviews_updated_at` equals the pull's current `updated_at`. The
required reconciliation rides the existing full-sweep clock, not a new retry
loop. Only a successful, complete bounded read may atomically replace the
collection, set version 1, set `standing_reviews_complete = true`, and copy the
pull's `updated_at` into `standing_reviews_updated_at`. Failure or a page beyond
the cap persists `complete = false`, keeps Merge unavailable, and retries on a
later full sweep. A complete response containing zero decisive reviews is
still a completed reconciliation and persists an explicitly empty collection.

This is how repair reopens review on evidence rather than on optimism: a new
commit invalidates acceptance, the new run replaces the attempt's observed
evidence, and the reviewer must accept that head. “Fix checks” never writes this
record.

### Check normalization

Comet derives the rollup from the contexts and keeps GitHub's rollup value for
diagnostics. The worse answer wins if they disagree.

- A CheckRun whose status is not `COMPLETED` is pending.
- `SUCCESS`, `NEUTRAL`, and `SKIPPED` conclusions pass.
- `FAILURE`, `TIMED_OUT`, `ACTION_REQUIRED`, `STARTUP_FAILURE`, `CANCELLED`,
  and `STALE` fail.
- A completed run with no conclusion is unknown.
- StatusContext `SUCCESS` passes, `PENDING` and `EXPECTED` are pending, and
  `FAILURE` and `ERROR` fail.
- An unrecognised value is unknown and is retained verbatim on its row.
- No contexts is `none`, which does not block a repository with no checks.
- Any failure wins over pending; pending wins over unknown; unknown or a
  truncated connection wins over passing. Only a complete set of passing
  contexts is `passing`.

Optional failures are still failures on this screen. `mergeStateStatus =
UNSTABLE` may permit GitHub to merge, but a reviewer who can see a failing test
should be offered repair before Merge, not silently taught that “optional” means
irrelevant.

### The dominant action

The core derives `RepositoryAction` and `verdict_controls`; clients only render
and invoke them. The verdict, repair, ready, and merge handlers recompute the
same gates on the board loop before writing anything, so a stale or hand-written
RPC cannot bypass a disabled control. Derivation starts with four current-pull
gates that outrank every cached check or stack summary:

1. aggregate `task_pr_merged = true` → no repository mutation; disable verdicts
   and hand the task to retention;
2. `layer_merged_at != null` while `task_pr_merged = false` → disable verdicts
   on the merged layer; complete topology gives `Open PR #N` for the lowest
   remaining open layer, while partial topology gives `Open stack on GitHub`;
   either way the task stays in Review and no retention clock starts;
3. `layer_state = CLOSED` with null `layer_merged_at` → `Open closed PR`;
   disable verdicts, and never retain an earlier Repair, Mark ready, or Merge
   action;
4. `changes_below = #N` → `Open PR #N`; disable all three verdict controls and
   every mutation on this row until the existing derivation returns it from
   `blocked` to `review`.

That fourth gate is the canonical replay boundary: Comment is disabled along
with Approve and Request changes because this diff is about to change. It
outranks a conflict or failed check on the current head; repairing or reviewing
that disposable head would only create work the ordered upstack replay moves
underneath.

For an open, reviewable row, inspect the current layer and every *visible* open
layer below it, bottom first. The lowest visible blocker owns the action. If it
is a lower layer, the action is `Open PR #N`; Comet never sends the current
layer's author a generic “the stack is red” prompt. The lower layer's own
Review screen offers the repair that belongs to its checkout. A blocker above
the current layer is shown on the map but does not block this layer by itself.

For the visible layer that owns the blocker, precedence is:

1. stale, truncated, or unknown repository/review state → `Open on GitHub` and
   say what could not be established;
2. merge conflict (`CONFLICTING` or `DIRTY`) → `Resolve conflict`;
3. failed GitHub Actions checks → `Fix failed checks`;
4. only non-Actions checks failed → `Open failed check`;
5. queued/running checks → `View running checks`;
6. draft, with no earlier blocker → `Mark ready` when Comet owns an authoring
   attempt, otherwise `Open draft`;
7. current layer not accepted for this head → no repository mutation; the
   enabled verdict controls are the dominant review act.

Only after no visible layer owns one of those blockers does topology decide
landing. `stack_gate = partial` produces `Open stack on GitHub`, says “showing
X of Y layers,” and cannot derive Merge: an unseen lower layer may be the real
blocker. Standalone or `stack_gate = complete` may derive `Merge…` only when the
current layer and every open layer below is accepted, non-draft, fresh,
untruncated, checks-complete, and known mergeable. Equal visible and GitHub
counts are necessary but not sufficient; the exact `1..=size` position check
defined above prevents duplicates from masquerading as completeness.

`BEHIND`, `BLOCKED`, and unrecognised `mergeStateStatus` values get their own
honest blocker text and an `Open on GitHub` action until a later design gives
them an owned mutation. They are not mislabeled conflict repair.

`Merge…` calls the `MergeTask` path from gh#408 after showing gh#408's exact
`merge_confirmation`. The confirmation carries the displayed
`expectedHeadSha` and `expectedTopologyFingerprint`. Before the existing
`MergeTask` executor is allowed to call `merge_pull_request`, its board-loop
handler performs an uncached preflight over the current layer and every layer
below it:

1. targeted REST reads refresh each layer's lifecycle, preview stack topology,
   head SHA, base ref/SHA, and `updated_at`;
2. the exact GraphQL selection above refreshes checks and mergeability for
   those node ids, in batches of 50;
3. a moved `updated_at` or incomplete standing-review marker triggers the
   complete bounded review reconciliation, so Comet-local standing requests
   and current-head approvals are rederived;
4. the board requires complete topology and every refreshed gate to pass, then
   recomputes the canonical fingerprint;
5. any gate failure or mismatch with either expected value persists the fresh
   projection and refuses before the merge executor runs.

Only an unchanged, freshly eligible landing slice reaches gh#408's executor in
that same serialized board-loop turn. No other function invokes GitHub's merge
endpoint, and GitHub still makes the final branch-protection decision. The
current-head guard remains, but is no longer asked to stand in for lower-layer
heads or bases. A race is a refusal and a fresh projection, never a merge whose
Comet-local acceptance was evaluated on different topology.

### Failed-check repair

Logs are demand-driven. Pressing `Fix failed checks` sends a request containing
the task, attempt, expected head, and exact failed check identities. The board
performs a targeted projection read first and refuses if the head, conclusion,
or check identity moved under the reviewer.

Only failed CheckRuns whose check-suite app slug is `github-actions` are log
candidates. For at most four, in the same priority order the screen shows:

1. read `GET /repos/{owner}/{repo}/actions/jobs/{check_run.database_id}` and
   verify `id`, `head_sha`, `name`, `status`, `conclusion`, and `html_url`
   against the observed check;
2. read `GET /repos/{owner}/{repo}/actions/jobs/{job_id}/logs`;
3. follow GitHub's one-minute `Location` only over HTTPS and without forwarding
   the Authorization header to the download host;
4. stream at most 8 MiB per job into memory, retaining a 32 KiB header and a
   224 KiB rolling tail; stop and mark the capture when the transfer cap is
   reached;
5. strip ANSI/control sequences, cap a line at 8 KiB, redact, and only then
   write at most 256 KiB per job. Four logs plus the manifest are capped at
   1 MiB.

The App needs `Checks: read`, `Commit statuses: read`, and `Actions: read` in
addition to its existing pull-request access. Missing permission is a visible
action refusal and a `doctor` finding; it does not queue an agent with a promise
of logs that were never captured.

#### Redaction and storage

GitHub's own `***` masks are preserved. Before anything reaches disk, Comet
also replaces:

- every board/App/member token or private-key value currently loaded by the
  process;
- GitHub token prefixes, bearer/basic authorization values, JWT-shaped values,
  URI user-info, cookies, and PEM blocks;
- assignment or JSON values whose key contains `TOKEN`, `SECRET`, `PASSWORD`,
  `PASSWD`, `API_KEY`, `PRIVATE_KEY`, `COOKIE`, or `AUTH`, case-insensitively.

The raw body is never logged, journaled, put in the prompt, or written to a
temporary file. Tests use synthetic secrets and fixtures only. Redaction count,
input bytes, output bytes, and both truncation flags go in the manifest so a
reader knows how partial the artifact is.

Artifacts live under the checkout's worktree-specific Git metadata, resolved
with:

```text
git rev-parse --git-path comet-board/repairs/<delivery-id>
```

That path is writable by a dispatched agent, follows the checkout when its Git
metadata is addressed, disappears with worktree collection, and never dirties
`git status`. Files are created mode `0600`, through a same-directory temporary
file and atomic rename, with generated digest/numeric names only. The manifest
ties every artifact to repository, PR number, task, attempt, head SHA, check-run
id, Actions job id, name, conclusion, details URL, observation time, and capture
time.

The prompt contains the manifest path and a short table of captured/omitted
checks, not the log contents. The agent is told to inspect the artifacts, make
the smallest repair in this checkout, rerun the relevant checks locally,
commit, push, update claims, and stop without approving or merging.

### Conflict repair

`Resolve conflict` uses the same delivery path without a log artifact. Its
preflight verifies current head SHA, base ref, base SHA, and conflict verdict.
The brief names the exact current layer and base, whether that base is trunk or
the layer below, and asks the author to fetch, resolve in this checkout, run the
relevant tests, commit, push, and update claims. It does not choose merge versus
rebase for the repository and it never force-pushes from the board process.

A parent conflict is repaired from the parent's Review and parent checkout. A
child that is clean against its parent says so while naming the parent blocker.
A child conflict is repaired in the child's checkout. These states are never
flattened:

```text
#47  conflict with main          Open #47 / Resolve conflict there
#48  clean against branch #47    waiting on #47
#50  checks failing on its head  independent; does not block landing #48
```

After a lower repair moves its head, the existing changes-below/upstack replay
semantics still apply. gh#407's direct child owns that replay; this feature does
not ask every layer to repair itself in parallel.

### Once, into the authoring checkout

Both repair kinds use `review::authoring_attempt` and
`still_the_authors_checkout`. A missing, archived, or repointed chat disables
repair and explains why; Comet does not dispatch a replacement and does not
guess from branch names.

The delivery id is a stable hash:

```text
checks:  task + attempt + head_sha + sorted(check_run_id, conclusion, completed_at)
conflict: task + attempt + head_sha + base_sha + base_ref + conflict verdict
```

`Runtime::prompt_once(chat, delivery_id, text)` extends the existing prompt
seam. `CometRuntime` uses `delivery_id` as both the durable command id and the
message id. `DocHost::queue_command_with_id` returns the existing command when
that id is already in the session document. The command ledger therefore
closes both crash windows: retrying after enqueue cannot create another turn,
and recording a receipt after enqueue cannot claim delivery before the durable
command exists.

`RepairReceipt` records who requested it, the task/attempt/head, kind, chat,
command id, artifact manifest, and whether this call or an earlier one queued
it. Repeated clicks on the same failure return that receipt. A rerun with a new
check id or a push with a new head is new evidence and earns a new delivery id.

The runtime steers a live author or starts the next turn in the idle authoring
chat, exactly as review delivery does now. The existing session event fast path
sees the settled attempt working and `rewatch_settled_attempts` reopens it — no
new attempt, branch, checkout, account, author identity, or MCP configuration.
When the turn ends, existing settle/evidence/claims logic closes it back into
Review. A no-op repair returns with the same failed head visibly still failed;
delivery is not success.

### Draft, merge, and retention mutations

`Mark ready` is a GraphQL `markPullRequestReadyForReview` mutation with the
stored pull-request node id and a deterministic `clientMutationId`. It is
offered only for a PR tied to an authoring attempt; an undispatched person's
draft gets `Open draft` instead. The board's GitHub credential performs this
author-side lifecycle mutation. It records no verdict and starts no agent turn.
The expected-head guard runs before it, and the mutation response refreshes the
projection immediately on the board loop.

`Merge…` remains a reviewer-side, explicitly confirmed call to gh#408. Repair
and Mark ready cannot reach it, cannot set its confirmation bit, and cannot
record approval. The reviewer identity resolution from gh#369 remains the only
path that casts a verdict.

Each `RepositoryGate` records when its own pull request merges, but that fact
does not settle a stacked task. The existing stack join continues to derive
aggregate `task.pr_open` from whether any layer remains open and
`task.pr_merged` only when the whole stack has landed. A bottom-layer merge with
an upper layer open therefore remains Review, preserves the chat/worktree, and
offers `Open PR #N` for the lowest open layer when topology is complete (or
`Open stack on GitHub` when it is not).

Only once aggregate `task.pr_merged` becomes true does the task derive `done`
and hand off to the existing `finish_on_merge`, `archive_chats`, and worktree
retention paths:

- `archive_chats = "on-settle"` normally archives the board-managed chat in
  that cycle;
- a duration shows the retention time and lets the existing sweep archive it;
- `off` says the route keeps the chat; a reviewer may use the ordinary,
  reversible Archive chat control;
- an adopted, non-board-managed chat or checkout is never archived or collected
  by this feature.

There is no second “merged cleanup” policy inside Review.

### RPC and ownership boundaries

The implementation adds two forwardable board RPCs:

- `RepairReview { taskId, attempt, kind, expectedHeadSha, checkIds }` →
  `RepairReceipt`;
- `MarkPullRequestReady { taskId, expectedHeadSha }` → mutation receipt.

The existing verdict and merge surfaces become:

- `SubmitVerdict { taskId, attempt, kind, comment, expectedHeadSha }` — the head
  is required for Comment, Approve, and Request changes alike;
- `MergeTask { taskId, confirmation, expectedHeadSha,
  expectedTopologyFingerprint }` — both optimistic-lock values are required
  when invoked from Review.

Desktop and iOS submit the values held by the rendered Review, never a fresh
client-side lookup. The CLI prints `head_sha` in `review` and requires
`verdict --expected-head-sha <sha>`; automation reading JSON passes that same
field. This makes a human's reviewed head explicit on every path rather than
letting the CLI silently bless whichever head happens to be current when Enter
arrives.

`ReadAttemptReview` gains the projection and derived action. Verdict, repair,
ready, and merge preflights execute on the board loop, the sole `board.db`
writer, and publish refreshed rows before returning a refusal. No failed
expected-head comparison records a verdict or queues a delivery.

The CLI mirrors the remote-capable operations as `repair --checks`, `repair
--conflict`, and `ready`; it does not gain another merge verb. JSON exposes the
same receipts the frontends receive.

Authority stays separated by construction:

- verdicts are reviewer facts and use the existing verified reviewer identity;
- repair is a durable message to the verified authoring attempt, never a
  verdict or PR write;
- Mark ready is an author-side stage change under the board opener's credential,
  never an approval;
- merge is still gh#408's explicit reviewer confirmation and single executor.

### Failure semantics

- A partial/stale projection is visible and action-ineligible, not clean.
- Absence from the 100-item pull list changes no persisted lifecycle; only a
  targeted identity response can close an omitted pull request.
- A changed head/base/check set refuses the stale action and asks the screen to
  reload.
- A verdict expected-head mismatch is side-effect free: no standing decision,
  author delivery, or GitHub post.
- A merge topology mismatch or refreshed lower-layer blocker never reaches
  gh#408's merge executor.
- A merged layer with aggregate `pr_merged = false` cannot start any task
  retention path.
- A log fetch or redaction failure writes no partial raw artifact and queues no
  repair turn.
- A captured subset names every omitted check and every truncation.
- An unreachable author leaves the PR facts intact and says nobody was told.
- A duplicate repair returns the first receipt.
- A repair turn that errors follows the existing blocked/failed attempt rules;
  it is never converted to a red check result by UI inference.
- GitHub refusing Mark ready or Merge returns GitHub's words. Neither is
  recorded as successful locally.

### Tests that make the contract executable

The implementation is complete when these seams are pinned:

1. A GraphQL fixture containing CheckRun and StatusContext values round-trips
   into the exact rollup above; unknown and truncated values fail closed.
2. Call-count tests prove missing/pending heads ride the configured sync cycle,
   stable heads ride the existing 120-second cadence, and 100 unchanged PRs
   take two serial batches rather than 100 per-PR requests. With 201 overdue
   identities, four batches refresh 200 in the first cycle and the remaining
   identity is selected next cycle rather than starved; continuous pending
   traffic still leaves the reserved oldest-overdue batch intact.
3. A repository fixture returns 100 newer pull requests on the list while an
   older persisted-open PR is omitted and closes. Its targeted node refresh
   observes `CLOSED` beyond the first page, removes stale Repair/Merge, and does
   not require list pagination.
4. A REST head change invalidates projection and approval before the next
   GraphQL answer.
5. A reviewer renders SHA A, SHA B arrives, and delayed Approve(A) performs a
   targeted refresh, compare-and-refuses, publishes B, and records/delivers/posts
   no verdict. Comment and Request changes run the same expected-head test.
6. Review fixtures keep decisions per reviewer: A's approval cannot clear B's
   change request; B's later approval clears only B's request; dismissing B's
   exact latest review clears B without changing A; dismissing an older B
   review leaves B's newer decision standing. An approval for SHA A never
   derives Merge on SHA B, while a fresh approval on B does when no reviewer
   still requests changes.
7. A pagination fixture with more than the review cap yields
   `review_state_truncated`, `acceptance = unknown`, and no Merge. Migration
   from an old `Delivered` with unchanged pull `updated_at` bypasses the normal
   short circuit; a complete zero-review response persists version 1,
   `standing_reviews_complete = true`, and an explicitly empty collection.
8. Every action-precedence row is table-tested, including no-check
   repositories, drafts, optional failures, stale data, and a REST transition
   from an actionable open projection to closed-unmerged. That transition
   yields only `Open closed PR`, disables verdict controls, and proves stale
   Repair and Merge actions cannot survive it.
9. Stack fixtures cover parent conflict/current clean, parent failure/current
   passing, current conflict, current failure, an unknown lower layer, and a
   failure above the current layer. A `changes_below = #N` fixture asserts all
   verdict controls and mutations are disabled and the sole action opens #N.
   A partial fixture with GitHub size three and only two unique visible layers
   asserts `stack_gate = partial`, `Open stack on GitHub`, and no Merge even
   when every visible gate passes. Each blocker fixture names its owning layer.
10. A complete bottom-merged/upper-open stack keeps aggregate `pr_open = true`
    and `pr_merged = false`, stays in Review, offers the lowest open upper PR,
    and invokes none of `finish_on_merge`, chat archive, or worktree collection.
    Only the final layer landing flips aggregate `pr_merged` and enters those
    existing paths.
11. A merge confirmation rendered with lower head A is delayed while that
    lower layer moves to B and the current head stays unchanged. The uncached
    preflight refreshes every gate, changes the topology fingerprint, refuses,
    and never calls gh#408's merge executor. A same-head check rerun and a new
    standing change request likewise fail the refreshed gate before the call.
12. Log fixtures prove transfer, artifact, line, job-count, and aggregate caps;
   ANSI stripping; every redaction class; mode `0600`; atomic writes; and a
   clean `git status`.
13. Repair preflight refuses moved heads and rerun checks. Double clicks, RPC
   retry, and a crash after durable enqueue all produce one command/message.
14. A repair starts the same chat/worktree/account configuration, reopens the
   settled attempt through the existing status path, and returns to Review only
   after a new run end.
15. Repair never changes `StandingReview`, `pr_changes_requested`, draft, or
    merge state. Mark ready never records a verdict. Merge still reaches only
    `SyncEngine::merge_pull_request` through `MergeTask` and the existing
    confirmation.
16. Aggregate-merged rows follow each existing chat/worktree retention choice,
    including the non-board-managed exemption; per-layer merged rows do not.

The GitHub facts used here are documented by GitHub's
[GraphQL pull-request schema](https://docs.github.com/en/graphql/reference/pulls),
[REST check-run API](https://docs.github.com/en/rest/checks/runs),
[commit-status API](https://docs.github.com/en/rest/commits/statuses), and
[Actions job-log API](https://docs.github.com/en/rest/actions/workflow-jobs).
The job-log endpoint returns a short-lived redirect to a plain-text log; it is
fetched only on the reviewer action above. The implementation should pin API
fixtures captured from Comet's own repositories, with names and log contents
replaced by synthetic values.
