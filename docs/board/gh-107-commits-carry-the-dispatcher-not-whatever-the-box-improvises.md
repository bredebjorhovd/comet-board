# Commits carry the dispatcher, not whatever the box improvises — **done** (gh#107)

The box had no `git config user.*`. Git does not stop at that: the first
dispatched agent invented an author, the commits went up under an address
belonging to no GitHub account, and Vercel's contributor check refused to deploy
the push. Everything downstream of the commit — attribution, the deploy gate,
`git log` — was reading an identity nobody set.

Three parts, in the order somebody meets them:

- **`doctor` reports the box's identity** (`git identity`). Read with `git -C
  <config dir> config --get user.name/user.email`, deliberately from outside any
  checkout: a repo-local override answers about that repo, and the question is
  what the *next* worktree the board cuts inherits. No name or no email is the
  one state that FAILs — an anonymous box is not a preference, it is a box
  nobody finished setting up — and the failure names the command to fix it. A
  `<id>+<login>@users.noreply.github.com` address passes naming the account it
  attributes to. Anything else passes **with guidance**, never a failure: whether
  an address is on an account's verified list is `GET /user/emails`, a
  user-scoped call the board's App may not make, and failing every operator who
  uses their real work address would be the gh#96 false alarm from the other
  side. The box wizard (`scripts/box-setup-wizard.sh`, tracked here as of this
  change) gained the matching stage, so a fresh box is pinned before it ever
  dispatches.
- **Per-dispatch authorship.** The attempt already recorded `dispatched_by_user`
  (gh#74); `[users]` in `routing.toml` maps that identity to a git author
  (`"ana@example.com" = "22494697+ana@users.noreply.github.com"`, or the
  `Name <email>` form git itself prints), and `build_spec` resolves it at
  dispatch time — the agent doing the committing knows nothing about who
  released it. From there it rides exactly where the push credential rides
  (gh#68): onto the chat config, so the fix for a review comment next week is by
  the same person as the first commit, then onto the harness child as
  `GIT_AUTHOR_NAME`/`GIT_AUTHOR_EMAIL`. **Author only** — the committer stays
  the box's pinned identity, which is what actually happened and what GitHub
  renders as *authored by them*. A file rather than a directory service because
  a two-person box does not need one, and because nothing on the box can answer
  "which GitHub account is this person?" on its own. Hand-written until §gh#162,
  which added `comet-board member add` and the page that says when to run it.
- **The two halves are independent.** An author with no App credential still
  authors (it just pushes as the box user); a credential with no `[users]` entry
  pushes as the App and commits as the box, which is what every board did before
  this. `doctor`'s `dispatch authorship` line prints in both states for the
  reason the duration cap does: "everything lands as the box" and "the map is
  working" look identical on GitHub until somebody reads the commit list.

**Why this reaches deploys at all.** Vercel attributes a deployment to the
commit's *author*, and on a team plan that attribution is a gate: an author
address that resolves to no GitHub account — or to an account that is not a team
member — can have its deploy refused rather than queued, and the failure appears
on the deployment, not on the push. Two settings decide the rest, both on
Vercel's side and neither of them ours to set: whether deploys created by a Git
*bot* (an App-authored commit, which is what a board pushing under its App can
produce) are built at all, and who counts as a contributor. What the board owes
them is a truthful, linkable author on every commit, which is what this section
is. A commit authored by a mapped teammate satisfies the gate *for that
teammate* — which is the point: their work deploys under their name, not the
box's.

Nothing here is authority. A commit author is a claim anybody can write by hand,
`dispatched_by_user` is unverified provenance (§gh#74), and neither decides what a
run may spend or push — that stays the explicit `account` (gh#59) and the board's
own App credential (gh#58).
