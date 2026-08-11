# Surviving the parent's merge — **done** (gh#286)

Stacks 5/9, on top of gh#285. When a layer of a stack lands, GitHub rebases
every layer above it onto the new base and retargets their pull requests —
**server-side**, in a repository this box only polls. Nothing on the box moves.

Three things stop being true in that moment, and the box was quiet about all
three. They are one event with three consequences, so they landed together in
`crates/board/src/rebased.rs`.

### 1. The stamped base stops describing the branch

`attempts.base_sha` is the commit the checkout was cut from, and for a stacked
layer that is the parent's tip. Everything the board measures off a branch is
measured from it: whether the agent produced anything, what the branch changed,
what a claim's remainder is (`gh#183`), how many call sites moved (`gh#236`).

The stamp is right for as long as the local branch is. It stops being a base at
all the moment anything rebases that branch — which for a stacked layer is
routine: the agent runs `gh stack sync` or `git pull --rebase` to catch up with
what GitHub already did. From then on `<stamp>..HEAD` is not this attempt's
work. It is the layer below's commits, plus every unrelated merge trunk has
taken since, plus this attempt's — and the review screen attributes the lot to
the child.

`SyncEngine::attempt_base` is the fix, and it is a check rather than a rule: is
the stamped commit still an ancestor of HEAD? Nearly always yes, and the stamp
is returned untouched. When it is not, the branch has been rewritten under it,
and `git merge-base` against the branch's *current* base names where it starts
now — the base GitHub last reported for the pull request, else the parent
attempt's branch, else `origin/HEAD`. Every reader of a base goes through it.

**Open question 2 — re-stamp on retarget, or teach the counting to use a merge
base?** Neither on its own. The stamp is re-stamped, but on evidence read off
the checkout and never on the observed retarget: a pull request moved on GitHub
says nothing about a local branch that has not moved, and a stamp re-pointed on
the strength of it would describe a rebase that has not happened. The check is
what makes that safe, and it is the cheap half — one `merge-base --is-ancestor`
per measurement, and the merge base only on the branch that failed it.

### 2. The child worktree never moves, and the next push is a force-push

`Repos::create_worktree_on` refuses to touch an existing branch, which is the
right instinct — an agent may be mid-flight in that checkout. So after GitHub's
rebase the checkout holds the pre-rebase history, and a `git push` from it puts
the old commits back over GitHub's work, undoing the rebase for the whole stack
above it.

**Open question 1 — is "no live chat in the checkout" enough to auto-rebase it?**
No, and the board does not rewrite an attempt's branch at all. The strongest
signal available is `review::still_the_authors_checkout`, which asks whether a
*chat's cwd* is that directory: a chat whose run ended still answers yes, a
human with the checkout open answers nothing at all, and a rebase that stops on
a conflict leaves a half-rebased worktree — with the agent's commits reachable
only from `ORIG_HEAD` — for whoever opens it next. The board keeps its hands off
the history and spends what it knows on telling instead.

`note_rewritten_branches` compares the checkout's HEAD with what origin holds
and, when they have diverged, says so once: a prompt into the authoring chat
while that chat is still the checkout's, naming the branch, what a push would do
now, and the two commands that fix it (`gh stack sync`, or `git fetch` +
`git rebase`); and a log line either way.

Three things keep it cheap, in the order they apply: only attempts with a
`stacked_on` edge are considered at all; only after the layer below has landed
(the parent's request merged, **or** this child retargeted off the parent's
branch — either spelling of the same event); and the free remote-tracking ref is
consulted before GitHub is, with the API call made only when the ref cannot
answer and only until the notice has been given. It does not run at all without
a runtime: a read-only caller must not consume the one notice an agent gets.

### 3. GC deleted the parent's branch under the child

`Verdict::Collect` removes a checkout *and its local branch*. A merged parent
goes `Spent` the moment its issue closes, while the child cut from it is still
being written. Nothing was lost — the commits stay reachable from the child —
but the branch is the only remaining name for the history the child sits on, and
a `git log board/gh-11-parent..HEAD` in that checkout stopped working.

`rebased::Dependents` is the `stacked_on` edge read downwards, built once per
sweep from the whole board the way `stacks::Stacks` is, and `gc::standing`
returns `Held` while anybody stands on the branch — the same deferral `pr_open`
already had. The chat goes with it (`gc::chat_standing` is deliberately the same
rule): a parent whose layer is still being written is not a finished
conversation either, which is also what 8/9's feedback fan-out will need.

What ends the hold is the **child's own checkout being reclaimed**, not its run
ending: a closed child still has a branch a reviewer may check out and a retry
may land on. Chains unwind from the top and cannot deadlock — each layer is
freed by its own standing, and freeing it frees the one below. The build output
inside a parent's checkout is *not* held (gh#186's rule stands): what a child
stands on is history, and none of that is in `target/`.

A held checkout says so once in the log, naming the attempt that holds it —
otherwise "why is this branch still here a month later" is a question only the
stacking edge can answer, and only by hand.

### Not in this issue

Surfacing the rewrite on the board itself. The notice reaches the agent, which
is where it is actionable, and the log reaches the operator; a row chip for "the
remote moved under this" is a viewport change and belongs with the rest of the
stacking UX.
