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
  legacy_review_blocker     null | { token, changes_requested_id, projection }
  review_state_truncated
  checks                    passing | failing | pending | none | unknown
  contexts_truncated
  stack_gate                standalone | complete | partial
  visible_layer_count
  github_stack_size
  topology_fingerprint
  verdict_controls          enabled | enabled_legacy_replacement |
                            disabled_changes_below | disabled_closed
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
reviewer's merge confirmation and the board's final preflight, not a substitute
for refreshing the gates. It is a local token: GitHub's merge endpoint accepts
an expected current head but no expected base or lower-layer fingerprint, so the
confirmation must not present the whole landing slice as atomically locked.

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
  next_standing_review_sequence     next PR-local value, seeded above legacy watermarks
  legacy_review_blocker             null | LegacyReviewBlocker
  standing_reviews[]
  submission_tombstones[]

Submission v2 additions
  identity_version          2 (absent means legacy)
  fingerprint               v2:<digest>
  reviewer_key
  head_sha
  kind
  normalized_body_sha256
  delivery_state            pending | completed | not_required
  standing_review_sequence  null for Comment

StandingReview
  key                       github:<review_id> | comet:<submission_fingerprint>
  sequence                  immutable chronological PR-local integer
  owner                     comet | github
  submission_fingerprint    required when owner is comet
  github_review_id          null until/unless GitHub accepts it as a review
  reviewer_key              GitHub user id (login only if absent), else verified Comet identity
  kind                      approved | changes_requested | unknown
  disposition               active | dismissed
  head_sha
  submitted_at
  transport                 decisive_review | comment | unposted | imported

LegacyReviewBlocker
  token                     digest of the exact legacy state being guarded
  changes_requested_id      the pre-upgrade Delivered value
  submission_fingerprint    matching legacy fingerprint when one is identifiable
  projection                posted | posted_as_comment | unposted | unknown

SubmissionTombstone
  fingerprint               exact v2 identity
  attempt
  reviewer_key
  head_sha
  kind
  review_id
  delivery_state            completed | not_required
  projection
  posted_as
  compacted_at
```

The legacy token is SHA-256 over a canonical length-prefixed encoding of the
repository, pull-request number, `changes_requested_id`, and every field of the
matching legacy submission (or an explicit absent marker). It changes if the
blocker is reconstructed, projected, or otherwise replaced, and cannot confirm
a different pull request's migration.

The persisted identity for every new submission is versioned separately from
the old `verdict::fingerprint`. The core first resolves the verified
`reviewer_key`, checks `expectedHeadSha`, and normalizes the body by converting
CRLF/CR to LF and trimming leading and trailing Unicode whitespace. It then
hashes a canonical length-prefixed encoding of `(attempt, reviewer_key,
expectedHeadSha, kind, normalized_body)` with SHA-256 and stores
`v2:<digest>`. Length prefixes, rather than delimiters, make the encoding
unambiguous. This identity is used for Comment delivery/post idempotency as well
as the `comet:` key of a decisive standing review. The old three-field
fingerprint remains readable only as legacy state; it is never used to dedupe a
new request.

Thus the same reviewer retrying the same verdict on the same head gets the first
receipt, while that reviewer acting on a new head and a second verified reviewer
acting on the same head create distinct submissions. The board-owned body
marker carries this v2 identity so a GitHub review id recovered after a crash
can alias only the submission that produced it.

`MAX_SUBMISSIONS = 20` remains the bound on full, retryable ledger entries, but
v2 identities are not evicted with them. Before inserting entry 21, the board
moves the oldest fully settled entry — delivery completed or was not required,
and its projection is on GitHub — into an exact `SubmissionTombstone`. The
tombstone retains only the fields needed to reconstruct an idempotency receipt
with delivery/projection status and suppress both side effects; it retains no
comment body or payload. A retry found in either tier returns `recorded = false`
and never delivers or posts again. This applies uniformly to Comments, which
have no `StandingReview`, and to decisive verdicts.

`MAX_SUBMISSION_TOMBSTONES = 256` bounds that second tier per pull request.
Unsettled live entries are never compacted because a retry may still have
delivery or projection work to finish. If the 20 live slots contain no settled
entry, or the exact tombstone tier is full, a new distinct submission refuses
before recording, delivery, or posting instead of evicting an idempotency key.
Retries of either tier remain available. Pull-request terminal cleanup removes
both tiers with the existing `Delivered` retention; this adds no independent
sweep or clock.

A GitHub review read adds `commit_id`, `user.id`, and `user.login`. Its complete
result replaces only records with `owner = github`; Comet-owned decisions are a
separate durable partition and are never deleted merely because GitHub returned
no decisive reviews. Each imported decisive or dismissed review is keyed by its
immutable review id *before* the new-message watermark is applied. If that id
already appears as `github_review_id` on a Comet-owned submission, the imported
state overlays that logical record instead of creating a second decision. In
particular, a later `DISMISSED` response dismisses the aliased decision without
changing its Comet ownership. Board-marked `COMMENTED` reviews are read only to
recover that alias and transport state; an ordinary GitHub comment is never
imported as a decision.

The review endpoint is paged at 100 records, to a hard maximum of ten pages,
only when the existing pull `updated_at` gate says feedback moved. Once that
timestamp differs, the board persists `standing_reviews_complete = false`
before the read; a crash cannot leave the GitHub-owned partition labeled
current. More pages set `review_state_truncated`; that projection cannot derive
Merge. A rewritten `DISMISSED` response marks that exact review dismissed while
retaining its previous kind when known. Seeing a dismissal for the first time
may leave `kind = unknown`; the tombstone still has the review's identity,
reviewer, head, and ordering.

A Comet verdict request carries the `expectedHeadSha` the reviewer actually
saw. Before recording, delivering, or posting anything, the board performs a
targeted pull refresh. It compare-and-refuses unless that value equals the
current head. It also compares the returned `updated_at` with
`standing_reviews_updated_at`: when the timestamp moved, the schema is old, or
`standing_reviews_complete` is false, the preflight completes the bounded review
reconciliation above *before* allocating a local sequence. A failed or truncated
reconciliation persists `acceptance = unknown` and refuses with no submission,
standing review, chat message, or GitHub review. Thus a GitHub review that
already moved `updated_at` is ingested and sequenced before the later Comet
verdict even when the Comet request reaches the board first.

Only after that ordered preflight does the verdict submission record the same
expected SHA and the transport's verified reviewer identity; Approve and
Request changes also allocate the next sequence and create the Comet-owned
standing decision, while Comment does not. The `[users]` mapping resolves that
identity to GitHub's numeric user id where possible; a normalized login is the
legacy fallback, and the authenticated Comet subject is the stable fallback
regardless of which GitHub credential posts the copy. If the transport cannot
supply a verified subject, submission refuses before recording. Identity
resolution happens before the v2 fingerprint is computed; a caller-provided
display name and the posting credential never participate.

Every outbound create-review request attributes the review to the head the
reviewer saw:

```json
{ "event": "APPROVE | REQUEST_CHANGES | COMMENT", "body": "...", "commit_id": "<expectedHeadSha>" }
```

`Github::post_review` therefore accepts the expected head explicitly and
serializes it as `commit_id`. That includes the reviewer's credential, the board
credential, and the self-review `COMMENT` fallback; no credential retry may
omit or replace it. `commit_id` is attribution, not compare-and-set: GitHub may
accept a review of A after B has become the pull request's latest head. A
successful response aliases a returned review id to the local A-scoped
submission (or leaves the v2 marker to recover an omitted id). A refusal still
leaves the already durable local decision and any author delivery bound to A;
for a plain Comment only the submission and delivery exist. Other credential
failures may continue through the existing fallback order, but every attempt
uses the same `commit_id`.

After that projection path reaches *any* final outcome — posted, posted as
Comment, refused, transport failure, or exhausted credential fallback — the
board performs a targeted pull refresh and rederives the repository projection.
This is unconditional, not a success-only cleanup. If the refresh finds B, an
approval of A does not count for B and B stays `needs_review`; a change request
remains standing under the cross-push rule, and a Comment contributes no
decision. The board never automatically reposts on B. The final pull refresh
applies the same completeness rule as preflight: if its `updated_at` is not
covered by `standing_reviews_updated_at`, it runs the complete bounded review
reconciliation before calling repository state fresh.

If the pull refresh fails, the last projection remains visible but gains
`stale_reason`, `acceptance = unknown`, and no Repair, Ready, or Merge action
until a later successful read. If the pull refresh succeeds but review
reconciliation fails or truncates, the refreshed head is persisted while review
state and `acceptance` remain unknown and mutation-ineligible.

The receipt exposes `reviewedHeadSha = A`, nullable `currentHeadSha`, nullable
`headMoved`, and `repositoryFresh`. A successful refresh fills both current
fields; a failed refresh returns `currentHeadSha = null`, `headMoved = null`,
and `repositoryFresh = false`, so no surface guesses whether A is still current.
A known head with incomplete review reconciliation returns that SHA and movement
answer but still sets `repositoryFresh = false`.

The existing board-owned body marker is extended to carry the durable submission
fingerprint on every create-review request. When GitHub returns a review id —
for a decisive review or a `COMMENT` fallback — the board writes it onto the
Comet-owned record as an alias; if the process dies between the POST and that
write, reconciliation recovers the same alias from the marker. A later read of
that id updates the same logical decision. Alias recovery indexes both live
submissions and compacted tombstones by v2 fingerprint, updating a tombstone's
transport fields without making it live or repeating a side effect. A
`COMMENTED` overlay may update its transport metadata but never replaces the
Comet decision's kind. A verdict
projected only as a GitHub comment or left unposted remains a Comet-owned
decision, consistent with the existing rule that the board's verdict stands
even when GitHub refuses its copy. Ordinary comments do not create decisions in
this collection; the local verdict that caused a fallback comment already did.

Aggregation consumes the union of the freshly replaced GitHub-owned partition
and the preserved Comet-owned partition after aliases have been collapsed. It
is deterministic and per reviewer:

1. take the record with the greatest numeric `sequence`, even when it is
   dismissed; `submitted_at` is display metadata and the hash-derived key is
   lookup identity, never ordering;
2. the sole board writer allocates a strictly increasing sequence when a Comet
   submission or previously unseen GitHub review is first recorded. New reviews
   in one GitHub batch are allocated in `(submitted_at, numeric review_id)`
   order; an existing id and a later alias retain their original sequence;
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
numeric `sequence`, from an unresolved legacy blocker's preserved
`changes_requested_id`, or null, rather than mutated by whichever verdict
arrived last.

The schema marker makes migration distinguishable from a valid empty review
set. The exact pre-upgrade input includes `Delivered::review`,
`Delivered::changes_requested`, `Delivered::fanned_out`, and
`Delivered::submissions[]`, whose `Submission` contains only `fingerprint`,
`review_id`, `delivered`, `projection`, `refusal`, and `posted_as`. It contains
no head, verified reviewer, kind field, or original body. Migration does not
invent any of them and does not reinterpret the old fingerprint as a v2
identity.

The new sequence and the retained fan-out watermarks stay in one numeric domain.
Before importing or allocating any `StandingReview`, migration computes the
legacy high-water mark as the checked maximum of:

- `Delivered::review`;
- `Delivered::changes_requested` when present;
- every positive legacy `Submission::review_id`;
- every value in `Delivered::fanned_out`;
- and, when resuming an interrupted migration, every already stored standing
  sequence and `next_standing_review_sequence - 1`.

It atomically seeds `next_standing_review_sequence` to high-water plus one, then
allocates imported GitHub reviews and later Comet decisions from that counter.
Checked overflow leaves the schema incomplete and Merge unavailable. The
existing per-dependent `fanned_out` map is retained unchanged: an old watermark
still suppresses the old notice, while every new change request is necessarily
greater and therefore reaches each existing dependent once. Reconciliation and
aliasing never lower or reseed the counter.

On load, an absent/older `standing_reviews_schema_version` or false
`standing_reviews_complete` sets `acceptance = unknown` and bypasses
`plan_delivery`'s unchanged-`updated_at` return. Before fetching GitHub, the
board snapshots that exact legacy shape:

- A legacy approval, whether posted, comment-only, or unposted, can never make
  `acceptance = accepted`: its reviewed head and reviewer are unknowable. A
  decisive GitHub review may establish acceptance later only from the head and
  reviewer returned by GitHub.
- `changes_requested = Some(id)` creates a `LegacyReviewBlocker`, preserves
  `pr_changes_requested = id` for stack fan-out, and keeps
  `acceptance = unknown` and Merge unavailable. A matching legacy submission
  contributes its projection for explanation, but not invented identity.
- A complete GitHub read may remove that blocker only by reconstructing the
  exact `id` as a decisive or dismissed GitHub review with its real
  `commit_id` and reviewer. Zero decisive reviews, an unmatched id, an
  `Unposted` submission, or a `PostedAsComment`/`COMMENTED` review cannot prove
  withdrawal and leaves the blocker intact.

An unresolved blocker is shown as “Pre-upgrade change request; reviewer and
head unknown.” Comment remains available, but Approve and Request changes must
show a separate “Replace pre-upgrade decision” confirmation. The Review
response supplies the blocker's digest `token`; a decisive `SubmitVerdict`
without the matching replacement token compare-and-refuses. With it, the board
atomically records the new v2, head- and reviewer-scoped decision and removes
only that unchanged legacy blocker before delivery/posting. A new Request
changes remains blocking under known identity; a new current-head Approve may
establish acceptance only if no other reviewer still requests changes.

The unchanged-`updated_at` return is legal only when the schema is current,
`standing_reviews_complete` is true, and `standing_reviews_updated_at` equals
the pull's current `updated_at`. The required reconciliation rides the existing
full-sweep clock, not a new retry loop. Only a successful, complete bounded read
may atomically replace the GitHub-owned partition, apply aliases and dismissals
to matching Comet-owned records, perform the conservative legacy transition,
set version 1, set `standing_reviews_complete = true`, and copy the pull's
`updated_at` into `standing_reviews_updated_at`. Failure or a page beyond the cap
persists `complete = false`, keeps Merge unavailable, and retries on a later
full sweep. A complete response containing zero decisive reviews persists an
explicitly empty GitHub partition while preserving every new-schema Comet-owned
decision and every unresolved legacy blocker.

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

1. an unresolved `legacy_review_blocker` → no repository mutation; Comment is
   enabled, while Approve or Request changes is the explicit, token-guarded
   `Replace pre-upgrade decision` review act described above;
2. stale, truncated, or unknown repository/review state → `Open on GitHub` and
   say what could not be established;
3. merge conflict (`CONFLICTING` or `DIRTY`) → `Resolve conflict`;
4. failed GitHub Actions checks → `Fix failed checks`;
5. only non-Actions checks failed → `Open failed check`;
6. queued/running checks → `View running checks`;
7. draft, with no earlier blocker → `Mark ready` when Comet owns an authoring
   attempt, otherwise `Open draft`;
8. current layer not accepted for this head → no repository mutation; the
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
`expectedHeadSha`, `expectedTopologyFingerprint`, and repository `observed_at`.
For a stack landing more than one open layer it also says, before the explicit
confirm control: “After confirmation, Comet rechecks every landing layer.
GitHub locks the current pull-request head during submission, but cannot lock
lower heads or bases.” Before the existing `MergeTask` executor is allowed to
call `merge_pull_request`, its board-loop handler performs an uncached preflight
over the current layer and every layer below it:

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
that same serialized board-loop turn. The executor passes the reviewed head to
the provider, whose irreversible request is exactly:

```json
{ "merge_method": "merge", "sha": "<expectedHeadSha>" }
```

The `sha` field is required on every `merge-async` invocation, whether GitHub's
default action selects a direct merge or its merge queue; no fallback may call
the endpoint without it. `Github::merge_pr` therefore accepts the expected head
explicitly instead of constructing a head-unlocked request. GitHub refuses a
current-head push between preflight and the `PUT`, and the board refreshes the
projection rather than recording a successful merge. No other function invokes
GitHub's merge endpoint, and GitHub still makes the final branch-protection
decision.

The topology fingerprint has a deliberately narrower guarantee. It catches a
lower-head, lower-base, or topology change visible before the request is handed
off, but GitHub exposes no compare token for those values. An external change
after preflight and before the `PUT` can therefore race a stacked landing while
the current head remains unchanged. The confirmation surfaces that residual
instead of claiming atomicity GitHub does not provide. The merge receipt stores
`atomicity = current_head_only`, the preflight observation time, and the exact
fingerprint, and triggers an immediate targeted refresh of every landing layer;
the refreshed Review shows any topology movement alongside GitHub's result.

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

- `SubmitVerdict { taskId, attempt, kind, comment, expectedHeadSha,
  replacesLegacyDecision }` — the head is required for Comment, Approve, and
  Request changes alike; the optional legacy token is accepted only for a
  decisive verdict against the still-current blocker. Its receipt adds
  `reviewedHeadSha`, nullable `currentHeadSha`, nullable `headMoved`, and
  `repositoryFresh`;
- `MergeTask { taskId, confirmation, expectedHeadSha,
  expectedTopologyFingerprint }` — both optimistic-lock values are required
  when invoked from Review; its receipt exposes `atomicity`,
  `preflightObservedAt`, and `topologyFingerprint` rather than implying the
  local fingerprint was a GitHub compare-and-set token.

Desktop and iOS submit the values held by the rendered Review, never a fresh
client-side lookup. The CLI prints `head_sha` in `review` and requires
`verdict --expected-head-sha <sha>`; automation reading JSON passes that same
field. Replacing a legacy blocker additionally requires
`--replace-legacy-decision <token>`; no CLI or client infers consent from an
ordinary Approve. This makes a human's reviewed head and any migration override
explicit on every path rather than letting a surface silently bless whichever
state happens to be current when Enter arrives.

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
- A push after verdict preflight may still leave a successful GitHub review of
  the old `commit_id`, or GitHub may refuse that post. Either outcome remains
  bound to the old-head local submission; the mandatory final refresh keeps the
  new head unapproved and surfaces both SHAs in the receipt.
- A failed final verdict refresh marks repository state stale and acceptance
  unknown; it never returns cached A as a known-current head or leaves a
  mutation action enabled.
- A changed `updated_at` or incomplete review marker that cannot be completely
  reconciled prevents local sequence allocation and every verdict side effect.
- A legacy sequence high-water overflow or incomplete atomic seed keeps the
  schema incomplete; per-edge fan-out watermarks are never compared with a
  reset counter.
- A full live-plus-tombstone submission ledger refuses a new identity; it never
  buys space by making an older delivery or GitHub post repeatable.
- An absent or changed legacy-replacement token leaves the unresolved blocker
  and all verdict state untouched.
- A merge topology mismatch or refreshed lower-layer blocker observed during
  preflight never reaches gh#408's merge executor. A lower-layer change after
  preflight is the explicitly displayed `current_head_only` residual, not
  falsely reported as an atomic refusal.
- A current-head push after merge preflight is server-refused by `sha`; no
  credential or queue fallback retries without that guard.
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
   no verdict. Comment and Request changes run the same expected-head test. In
   the narrower compare-to-POST race, the initial compare passes on A and B
   arrives before GitHub receives the request: reviewer credential, board
   credential, and `COMMENT` fallback bodies all retain `commit_id = A`; the
   accepted-old-head fixtures alias each id only to the A-scoped submission,
   refresh B, and never derive B as accepted or repost on B. Refused-old-head
   fixtures across the same paths also run the final refresh and expose B. A
   pull-refresh failure after either result returns null current/head-moved
   fields, `repositoryFresh = false`, stale/unknown projection state, and no
   mutation action. A successful B refresh followed by truncated review
   reconciliation retains B in the receipt but likewise reports repository
   state not fresh and acceptance unknown.
6. Review fixtures keep decisions per reviewer: A's approval cannot clear B's
   change request; B's later approval clears only B's request; dismissing B's
   exact latest review clears B without changing A; dismissing an older B
   review leaves B's newer decision standing. An approval for SHA A never
   derives Merge on SHA B, while a fresh approval on B does when no reviewer
   still requests changes. Equal-timestamp approval/change-request pairs use
   the greater numeric sequence in both arrival orders; reversing the kinds at
   those sequence values reverses the standing decision, regardless of key
   sort order. An approval is created on GitHub first and moves `updated_at`,
   then a newer Comet Request changes reaches the board before GitHub ingestion:
   verdict preflight reconciles and assigns the approval sequence N before
   allocating N+1 to the local request, which remains the blocker. A failed or
   truncated version of that preflight allocates no sequence and performs no
   delivery or post.
7. A pagination fixture with more than the review cap yields
   `review_state_truncated`, `acceptance = unknown`, and no Merge. Migration
   fixtures deserialize the exact pre-upgrade `Delivered` JSON rather than
   constructing new `StandingReview` records. Serialized unposted and
   `posted_as_comment` change requests followed by a complete zero-decisive
   response retain a visible legacy blocker and `pr_changes_requested`; a
   serialized legacy approval never accepts an unnamed head. An exact decisive
   GitHub review id reconstructs the blocker, while an explicit matching-token
   replacement atomically swaps it for a v2 decision; absent/stale tokens
   refuse. Unchanged pull `updated_at` still bypasses the normal short circuit.
   A serialized stacked parent carries `review = 900`, an outstanding blocker,
   and `fanned_out[child] = 900`; after migration clears that blocker, a new
   local Request changes receives a sequence above 900. The existing child gets
   the new generic notice once, its edge watermark advances to the new sequence,
   and a second fan-out pass is silent.
8. V2 submission-identity fixtures prove an empty Approve retried by the same
   reviewer on the same head dedupes, the same reviewer on a new head is
   distinct, and two verified reviewers approving the same head are distinct.
   CRLF/LF and boundary whitespace normalize to one identity, while internal
   body changes do not; a legacy v1 fingerprint for otherwise identical input
   does not dedupe the v2 request. New-schema comment-only and unposted decisions
   survive a complete zero-review reconciliation. A locally posted decisive
   review later read by its GitHub id aliases to one decision rather than two,
   and dismissal updates that exact aliased decision. A `COMMENT` fallback
   aliases its review id for transport bookkeeping but remains the one
   Comet-owned decision; a crash after POST recovers the alias from its v2
   submission marker. A settled Comment is compacted by 20 later live
   submissions, survives a serialize/reload as an exact tombstone, and a retry
   performs neither chat delivery nor GitHub POST. The same assertion covers a
   decisive verdict; a full live-plus-tombstone ledger refuses a new identity
   instead of evicting the oldest one.
9. Every action-precedence row is table-tested, including no-check
   repositories, drafts, optional failures, stale data, and a REST transition
   from an actionable open projection to closed-unmerged. That transition
   yields only `Open closed PR`, disables verdict controls, and proves stale
   Repair and Merge actions cannot survive it.
10. Stack fixtures cover parent conflict/current clean, parent failure/current
   passing, current conflict, current failure, an unknown lower layer, and a
   failure above the current layer. A `changes_below = #N` fixture asserts all
   verdict controls and mutations are disabled and the sole action opens #N.
   A partial fixture with GitHub size three and only two unique visible layers
   asserts `stack_gate = partial`, `Open stack on GitHub`, and no Merge even
   when every visible gate passes. Each blocker fixture names its owning layer.
11. A complete bottom-merged/upper-open stack keeps aggregate `pr_open = true`
    and `pr_merged = false`, stays in Review, offers the lowest open upper PR,
    and invokes none of `finish_on_merge`, chat archive, or worktree collection.
    Only the final layer landing flips aggregate `pr_merged` and enters those
    existing paths.
12. A merge confirmation rendered with lower head A is delayed while that
    lower layer moves to B and the current head stays unchanged. The uncached
    preflight refreshes every gate, changes the topology fingerprint, refuses,
    and never calls gh#408's merge executor. A same-head check rerun and a new
    standing change request likewise fail the refreshed gate before the call.
    A current-head push after a successful preflight but before the `PUT` is
    refused by GitHub because the request still carries `sha = expectedHeadSha`.
    A lower-head move in that same post-preflight window is not asserted to be
    atomic: the fixture accepts the unchanged current head, records
    `atomicity = current_head_only`, immediately refreshes all landing layers,
    and exposes the changed fingerprint in Review.
13. Log fixtures prove transfer, artifact, line, job-count, and aggregate caps;
   ANSI stripping; every redaction class; mode `0600`; atomic writes; and a
   clean `git status`.
14. Repair preflight refuses moved heads and rerun checks. Double clicks, RPC
   retry, and a crash after durable enqueue all produce one command/message.
15. A repair starts the same chat/worktree/account configuration, reopens the
   settled attempt through the existing status path, and returns to Review only
   after a new run end.
16. Repair never changes `StandingReview`, `pr_changes_requested`, draft, or
    merge state. Mark ready never records a verdict. Merge still reaches only
    `SyncEngine::merge_pull_request` through `MergeTask` and the existing
    confirmation.
17. Aggregate-merged rows follow each existing chat/worktree retention choice,
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
