# What still needs testing, in one place (gh#382)

The manual test list, written down instead of drifting in the tracker. Every
item on it needs a human, a phone or a real sign-in — that is why none of it is
dispatchable, and why it kept sliding until it lived here. Tick the boxes as
you go; this file is the list's one home. The ticket beside each item is the
change being tested, not work to do.

Ordered so the early items unblock the later ones: **section A is one dispatch
that answers four questions**, so start there. Everything in it runs **on the
box** — a dispatch released from the Mac goes through `herdr-board`, a
different program out of a different repo, and proves nothing about any of
this. The same trap §gh#339 documents for its own verification applies here.

### A. One dispatch on the box, four answers

Pick any small open issue and release it on the box, then read the settled
attempt back:

```sh
ssh comet@62.238.106.136
comet-board list --state ready
comet-board dispatch --task gh:<owner>/<repo>#<n>
# wait for it to settle, then:
comet-board review --task gh#<n> --json | jq '{claims_at, sandbox, branch}'
```

- [ ] **1. `claims_at` is not null** *(§gh#339)*. Twenty-three consecutive
  attempts claimed nothing before v0.6.0. If it is still null the brief did not
  reach the agent — check the dispatch ran the 0.6.0 binary and not a stale one.
- [ ] **2. `--task gh#<n>` resolved at all** *(§gh#339)*. On v0.5.0 this
  answered `gh#<n> is not on the board`. If it still does, nothing else in
  section A is trustworthy.
- [ ] **3. The review names the sandbox** *(§gh#349)*. A Claude or opencode
  dispatch should say **"this agent had full access to the box"** — reported
  even though that is what was requested.
- [ ] **4. A Codex dispatch commits** *(§gh#349)*. Route one to `codex` and
  confirm it pushes. The interesting evidence is a run whose report says
  `workspace-write` **and** whose commits landed — before v0.6.0 that path
  silently escalated to full access on every worktree dispatch.

### B. The review surface (gh#264)

This is gh#264 itself, and it has never been run — it needs you to reject a
pull request halfway through, which is why. All four run from the review
window, on a board-dispatched pull request.

- [ ] **5. Approve, from the review window**, on a board-dispatched pull
  request. Expect it to land as a **comment that says it approves** — not an
  approving review — until a user token is configured *(§gh#365)*. The receipt
  says which: *"It is on the pull request as a comment that says it approves."*
- [ ] **6. Request changes**, same path, same expectation *(§gh#365)*.
- [ ] **7. A refused verdict is still recorded, still delivered into the
  checkout, and your typed comment survives** *(§gh#365)*. This is the one that
  used to eat your review — GitHub was the first writer, and a refusal there
  lost the record, the delivery and the comment together. Worth doing
  deliberately rather than noticing it later.
- [ ] **8. A `changes requested` reaches the agent still in the checkout**, and
  the layers stacked above go back to waiting *(§gh#289)*.

### B2. Only after you set a user token

On the box, in the board's `.env`. The login is already recorded in `[users]`
(§gh#162); the variable name is that login uppercased:

```
GITHUB_USER_TOKEN_BREDEBJORHOVD=<PAT, repo scope>
```

- [ ] **9. `comet-board doctor` — the `review identity` line** *(§gh#369)*. It
  should name who opens, say you can cast a verdict under your own name, and
  **not** FAIL on a collision — the one FAIL that line knows is one account on
  both sides.
- [ ] **10. Approve now lands as a real approving review**, under
  `@bredebjorhovd`, and the receipt says *"as @bredebjorhovd"* *(§gh#369)*.
- [ ] **11. `git log` on a dispatched branch still shows the right author**
  *(§gh#162)*. The token change must not have moved commit authorship —
  different axis, and the one thing that would be bad to get wrong quietly.
  Authorship is stamped on the harness child, not taken from any token
  (§gh#107), which is why these two must stay independent.

### C. Board behaviours that changed under you

- [ ] **12. A dispatching chat survives its own settle** *(§gh#354)*. Dispatch
  two agents from one pane, let both settle **and merge**, confirm the pane is
  still there. The first fix released the hold at settle — which is exactly
  when you lose it — so the merges are part of the test, not an afterthought.
- [ ] **13. An undispatched pull request is reviewable** *(§gh#344)*. Push a
  branch by hand, open a pull request, open the review on the `gh!<n>` row.
  Expect: no claims, the whole diff as the remainder, and **nothing** saying
  the attempt "never answered the contract".
- [ ] **14. Task names read as names** *(§gh#364)* — on the board, in
  `git branch`, and on the phone. Worth one look at a Norwegian title:
  `kjor-dryrun-altinn`, not `dryrun-av-altinn`.

### D. The edge, after last night

- [ ] **15. The row burn actually dropped** (gh#377 — the `setMeta` fix has no
  write-up of its own; the baseline is [gh-373](gh-373-what-the-edge-was-burning.md)).
  Compare `rowsWritten` per hour against a comparable active day — that
  document's hour table is what a runaway day looked like. Expect roughly a
  third.
- [ ] **16. The daily R2 backup still lands**, and `/stats` shows
  `alarm.consecutiveFailures` at 0 *(§gh#378)*.
- [ ] **17. Duration is still the dimension to watch** — 76% of the paid
  allowance *(§gh#373)*. A weekly glance at the namespace table is enough.

### E. Standing tickets that are themselves tests

- [ ] **18. gh#337** — the stacks sequence against a real GitHub stack on
  `board-scratch` *(§gh#337; `scripts/stacks-rig.sh` is the rig)*. Wants
  watching: change a **lower** layer and an **upper** one, and see how the
  retarget holds.
- [ ] **19. gh#53** — production convergence smoke. Needs a real WorkOS
  sign-in, which is why no agent can run it. It also independently verifies DO
  hibernation — §gh#373 argued it was correct from analytics; this confirms it
  live, which matters more now that hibernation is a cost fact.

### What would make this list shorter

Most of section A exists because nothing can assert *"a dispatched agent, on
the box, did X"* from a test suite. A **smoke dispatch** — a trivial task
released on the box, with assertions on the resulting row (claims present,
sandbox reported, branch named) — would collapse items 1–4 into one command
that runs itself. Worth building once this list has been walked manually and
it is clear which assertions actually matter.
