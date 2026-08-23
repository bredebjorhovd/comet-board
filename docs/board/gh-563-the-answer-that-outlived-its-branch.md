# The answer that outlived its branch (gh#563)

Review delivery into a live chat is the feature: a verdict on a pull request is
queued into the chat that wrote it, while that chat still stands in the
checkout. It works. This issue is about what happens on the other side of it,
and it happened three times in one session:

| task | what was stranded |
| --- | --- |
| gh#527 | a whole review finding implemented — 5 files, 244 insertions |
| gh#558 | a cutover writeup, including a trap that would have made a correct migration read as failed |
| gh#553 | nothing — only because the dispatcher held the merge |

The shape: an agent settles, its pull request merges, and *then* the verdict it
was handed produces work. The agent pushes to a branch whose pull request has
closed. Nothing opens. The board settles the attempt a second time — "settled
on commits · no pull request was opened" — in wording identical to a first
settle, and the commits sit on a dead branch until somebody diffs it against
main by hand.

### Why "review before merging" is not the fix

The dispatcher cannot know whether a verdict will produce a push. An approval
with a note may produce nothing; the same words may produce a commit. There is
no signal and no bounded window, so holding every merge against the chance of
an answer is not a workflow either.

The asymmetry was the bug: the board was willing to deliver a review into a
chat *after* the thing that review was about could no longer accept commits,
and had no move for what comes back.

### The rule

**A settled branch whose pull request has merged does not strand its answer.
The board gives the answer its own pull request, the way the first settle
would have.**

Detection was never missing. The second settle already ran exactly where the
bug lives — `Finished(Commits)`: run genuinely ended, no open pull request,
commits past base, on origin. What was missing was the action, so the settle
now takes it (`SyncEngine::reopen_after_merge`, event path only):

- **Open the pull request**, from the branch at the base the merged one used,
  and record it immediately — `set_pr` before anyone reads anything, so the
  row derives straight back into `review`.
- **Undo the merge's done.** `finish_on_merge` sets `local_done` as a
  mechanical consequence of merging; fresh reviewable work outranks it.
  Cleared only under `pr_merged`, so an operator's deliberate mark stands.
- **Reopen the issue** through the writeback queue (new `reopen` kind), since
  a closed upstream derives `done` over any open PR and the close was queued
  by the same merge.
- **Say it in every channel.** The settle's note slot carries the clause:
  *"pushed 2 commit(s) after o/r#50 had merged — reopened as o/r#99"*. The log
  line is a warning, not an info. The outcome comment upstream names the new
  URL. Nothing reads like a first settle any more.

### The guards

The action fires only on the reported shape, so nothing else changes meaning:

- Event path only (`ask_github`) — no poll pays for this.
- A recorded, *merged* PR on this attempt's own branch (`pr_head_ref`
  equality). A bare `mark done` is untouched.
- GitHub must say the branch still carries commits the base lacks
  (`compare_ahead`). Zero means the push raced the merge and lost — everything
  it made is already in main — and there is nothing to reopen. Unproven reads
  as "do not act", and every failure falls back to today's plain commits
  settle rather than failing one.

The compare is asked of GitHub because the local checkout cannot answer it:
the merge commit exists only there, and this path does not fetch.

### The other half: warn at the keypress

`merge` now checks whether a review reached the agent's chat within ten
minutes (`Delivered.last_delivery`, stamped by both the inbound pass and
verdict delivery — the window where an answer is most likely). If so, the
reply to whoever pressed the key grows a sentence: a review arrived just
before this merge, and an answer will be reopened as its own pull request.
The keypress is the moment somebody is watching; spending it on silence was
the expensive choice.

### Not in this fix

An answer that arrives as claims rather than commits settles nothing and
strands nothing — there is no artifact to lose. An agent woken after every
rewatch window has closed cannot happen in practice: delivery wakes the chat,
and the rewatch sees `Working` long before any merge lands. Both edges are
covered by the same settle path if they ever widen.
