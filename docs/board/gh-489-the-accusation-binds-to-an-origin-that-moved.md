# The accusation binds to an origin that moved — **done** (gh#489)

gh#468's attempt 3 was a repair run: dispatched onto a branch its previous
attempt had already pushed, it read, verified, and pushed nothing — its journal
holds no `git push` and no `gh pr create`. The engine handed the run a board
credential anyway, as it hands one to every run, and the settle then did the
gh#233 arithmetic on it: work is on origin, the credential was handed, the
helper never minted — "the board's credential did not push this." Every word of
which is true, and the sum of which is false: nothing pushed the branch
*during* this attempt at all. The evidence the settle rested on — the open
pull request, the remote branch — predated the run by an attempt.

The gap was that the check observed a **state** (the branch is on origin) where
it needed a **change** (origin moved while this attempt ran). Retry and repair
runs make the state true on their first second of life, so every quiet,
correct, review-only run on a pushed branch arrived at its settle pre-accused.

- **Dispatch stamps what origin already holds.** After the worktree is cut,
  beside the `base_sha` stamp, the engine records the commit origin's copy of
  the attempt's branch points at — GitHub asked directly when the board has a
  client, the checkout's remote-tracking ref otherwise
  (`SyncEngine::stamp_origin_at_start`, kept in `meta` under
  `origin_start:<attempt>`). A branch origin does not hold leaves no stamp,
  which reads the same as an attempt from before the stamp existed: any work
  on origin at settle is new.
- **The settle compares before it accuses.** `note_credential_path` still asks
  the ledger first — a minted run is quiet, an unhanded box is quiet, exactly
  as before. But an unsanctioned record now has to coincide with an origin
  that *moved*: the branch sitting exactly where the attempt found it means
  nothing was pushed by anybody, so there is no push to account for, and the
  settle notice carries no clause and the issue gets no comment. The log says
  why, once.
- **Everything that moved stays loud.** A changed OID — an ordinary push, a
  force-push (the comparison is identity, not ancestry, so a rewrite cannot
  slip past as "already there"), or a branch another actor moved mid-attempt —
  still demands a same-chat mint, and accuses without one. So does an attempt
  with no stamp, and one whose origin cannot be read at settle: unproven reads
  as accused, never as excused. The failure mode this feature is allowed to
  have is the old behaviour, not a new silence.
