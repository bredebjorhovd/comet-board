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

The verdict bar keeps the three review verdicts exactly as it has them. The
repository action occupies the existing quiet `Merge…` position and changes
with repository state. A repair is not a verdict, an approval, or permission to
merge. It starts another turn in the authoring chat and nothing else.

### One head, one observation

The new wire shape is deliberately about an immutable head:

```text
RepositoryProjection
  observed_at
  head_sha
  base_ref
  base_sha
  draft
  mergeable                 MERGEABLE | CONFLICTING | UNKNOWN
  merge_state_status        GitHub's value, retained verbatim
  github_review_decision    APPROVED | CHANGES_REQUESTED | REVIEW_REQUIRED | null
  acceptance                accepted | changes_requested | needs_review
  checks                    passing | failing | pending | none | unknown
  contexts_truncated
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

The task record grows `pr_node_id`, `pr_head_sha`, `pr_base_sha`, `pr_draft`,
`pr_repository_json`, and `pr_repository_observed_at`. The variable-length
contexts stay in the versioned JSON projection; the identity and head fields
stay flat because the sync, mutations, and stack join need them without
decoding a UI payload. Data written before these fields existed reads as an
unknown projection, never as a clean one.

`AttemptReview.repository` carries the full current-layer projection. A
`StackLayer` carries only its `RepositoryGate` summary — head, acceptance,
checks, draft, mergeability, freshness, and blocker — so a review can explain
each layer without duplicating every sibling's check list on the wire.

### Exactly what GitHub is asked

No new timer is added.

The existing board cycle is `[sync] interval` from `routing.toml`, 30 seconds by
default and clamped to at least five seconds. Its existing REST pull-list call
continues to run once per configured repository per cycle. The parser retains
these fields from each item:

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

The query is due for an open pull request when any of these is true:

- it has no projection;
- the REST list observed a different head or base;
- its last projection contains a pending context;
- its `observed_at` is at least the existing 120-second `FULL_SWEEP_SECS`
  cadence old.

Thus a running build updates on the ordinary board clock, while a stable,
unchanged pull request costs only its slot in a batched response every existing
full sweep. There is no per-PR poller, no task spawned by opening Review, and no
background log fetch. Fifty ids, 100 contexts per id, and the pull list's
existing 100-item repository ceiling bound a cycle. `hasNextPage` sets
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

The existing review-feedback read gains `commit_id` and `user.login` from pull
request reviews. It already rides `updated_at` watermarks and therefore adds no
call. `reviewDecision` is displayed as GitHub's branch-policy answer; Comet's
authority is the head-scoped standing verdict below.

### Acceptance belongs to the reviewed head

Today `Delivered::changes_requested = None` means both “approved” and “nobody
approved.” That cannot drive a merge action. `Delivered` therefore gains a
standing review record:

```text
StandingReview
  kind                      approved | changes_requested
  review_id
  head_sha
  submitted_at
  source                    comet | github
  reviewer
```

A Comet verdict records the projection's current `head_sha` before it is
delivered or posted. An inbound GitHub review records its `commit_id`. Comments
do not replace the standing decision; dismissed reviews withdraw the decision
they dismissed. The existing `pr_changes_requested` mirror remains the stack
fan-out key, while `StandingReview` answers the richer question.

`acceptance` is `accepted` only when the last standing approval names the
current head and no standing change request remains. A push changes the head,
so an approval of the old head becomes `needs_review` immediately even in a
repository configured not to dismiss stale GitHub approvals. Existing records
with no `head_sha` do not grandfather themselves into approval.

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

The core derives `RepositoryAction`; clients only render and invoke it. For a
stack, inspect the current layer and every open layer below it, bottom first.
The lowest blocker owns the action. A blocker above the current layer is shown
on the map but does not stop this layer landing.

If the blocker is a lower layer, the action is `Open PR #N`. Comet never sends
the current layer's author a generic “the stack is red” prompt. The lower
layer's own Review screen offers the repair that belongs to its checkout.

For the layer that owns the blocker, precedence is:

1. merged → no repository mutation; hand off to retention;
2. stale, truncated, or unknown → `Open on GitHub` and say what could not be
   established;
3. merge conflict (`CONFLICTING` or `DIRTY`) → `Resolve conflict`;
4. failed GitHub Actions checks → `Fix failed checks`;
5. only non-Actions checks failed → `Open failed check`;
6. queued/running checks → `View running checks`;
7. draft, with no earlier blocker → `Mark ready` when Comet owns an authoring
   attempt, otherwise `Open draft`;
8. not accepted for this head → no repository mutation; the existing verdict
   controls are the dominant review act;
9. accepted, non-draft, complete checks, known mergeable state, and every open
   layer below satisfying the same gate → `Merge…`.

`BEHIND`, `BLOCKED`, and unrecognised `mergeStateStatus` values get their own
honest blocker text and an `Open on GitHub` action until a later design gives
them an owned mutation. They are not mislabeled conflict repair.

`Merge…` calls the `MergeTask` path from gh#408 after showing gh#408's exact
`merge_confirmation`. No other function invokes GitHub's merge endpoint. The
call includes the projection's expected head as a compare-and-refuse guard; the
existing merge implementation checks it before `merge_pull_request`. GitHub
still makes the final branch-protection decision. A race is a refusal and a
fresh projection, never a merge of an unreviewed head.

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

Once GitHub reports `merged`, the task derives `done` and the existing
`finish_on_merge`, `archive_chats`, and worktree retention paths own the rest:

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

`MergeTask` only gains an optional expected-head guard. `ReadAttemptReview`
gains the projection and derived action. All three mutations execute on the
board loop, the sole `board.db` writer, and publish rows before replying when
their local state changes.

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
- A changed head/base/check set refuses the stale action and asks the screen to
  reload.
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
   take two serial batches rather than 100 per-PR requests.
3. A REST head change invalidates projection and approval before the next
   GraphQL answer.
4. An approval for SHA A never derives Merge on SHA B; a fresh approval on B
   does.
5. Every action-precedence row is table-tested, including no-check repositories,
   drafts, optional failures, stale data, and closed-unmerged PRs.
6. Stack fixtures cover parent conflict/current clean, parent failure/current
   passing, current conflict, current failure, an unknown lower layer, and a
   failure above the current layer. Each names the layer that owns the action.
7. Log fixtures prove transfer, artifact, line, job-count, and aggregate caps;
   ANSI stripping; every redaction class; mode `0600`; atomic writes; and a
   clean `git status`.
8. Repair preflight refuses moved heads and rerun checks. Double clicks, RPC
   retry, and a crash after durable enqueue all produce one command/message.
9. A repair starts the same chat/worktree/account configuration, reopens the
   settled attempt through the existing status path, and returns to Review only
   after a new run end.
10. Repair never changes `StandingReview`, `pr_changes_requested`, draft, or
    merge state. Mark ready never records a verdict. Merge still reaches only
    `SyncEngine::merge_pull_request` through `MergeTask` and the existing
    confirmation.
11. Merged rows follow each existing chat/worktree retention choice, including
    the non-board-managed exemption.

The GitHub facts used here are documented by GitHub's
[GraphQL pull-request schema](https://docs.github.com/en/graphql/reference/pulls),
[REST check-run API](https://docs.github.com/en/rest/checks/runs),
[commit-status API](https://docs.github.com/en/rest/commits/statuses), and
[Actions job-log API](https://docs.github.com/en/rest/actions/workflow-jobs).
The job-log endpoint returns a short-lived redirect to a plain-text log; it is
fetched only on the reviewer action above. The implementation should pin API
fixtures captured from Comet's own repositories, with names and log contents
replaced by synthetic values.
