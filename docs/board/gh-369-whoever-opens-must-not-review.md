# Whoever opens a dispatched pull request must not be whoever reviews it (gh#369)

§gh#365 made a refused verdict survive, and made an approval GitHub will not
take arrive as a comment that says it approves. That is the honest thing to do
with one credential. This is the other half: **making GitHub take it.**

GitHub refuses `APPROVE` and `REQUEST_CHANGES` on a pull request the caller
opened. So the identity that **opens** a dispatched pull request and the
identity that **casts** the verdict have to be two accounts. Verified live on
both boards, failing in opposite directions:

| | opens the PR | casts the verdict | |
| --- | --- | --- | --- |
| the box (`comet-board`) | `app/comet-board` | `app/comet-board` | 422 |
| this Mac (`herdr-board`) | `bredebjorhovd` | `bredebjorhovd` | 422 |

The split is the same on both machines, and it is not a preference:

- **The bot opens.** The work is the agent's, and the author of record should be
  an identity no human ever reviews as.
- **The human reviews.** A verdict is a person's act, and an approving review
  with a person's name on it is the only version of one that means anything to
  somebody reading the pull request later — or to branch protection.

Landed in `crates/board/src/config.rs` (`GITHUB_USER_TOKEN_*`,
`RoutingConfig::github_login_for`), `crates/board/src/verdict.rs`
(`SyncEngine::reviewer_credential`, `project_verdict`, `posted_as`),
`crates/board/src/sources/github.rs` (`AsUser`, `Github::viewer`),
`crates/engine/src/rpc.rs` (`Server::reviewer`), and `doctor`'s new
`review identity` line.

### The credential

A member's own GitHub token, in the board's `.env`, named after the login their
`[users]` entry already resolves to:

```
GITHUB_USER_TOKEN_BREDEBJORHOVD=github_pat_…
```

`[users]` (§gh#162) is where a board member's GitHub account is already
recorded, so nothing new had to be invented to say *whose* token this is — only
where the secret goes, which is never `routing.toml`. The variable name is the
login uppercased with hyphens written as underscores, which is total and
injective: a login is ASCII alphanumerics and hyphens, so `a_b` is not a login
anybody can have and `A_B` can only have come from `a-b`.

Only a GitHub-minted noreply address answers `github_login_for`. Any address is
a fine *commit* author — GitHub attributes to whichever account holds it — but
choosing whose credential casts a review is not a question to answer by
inference, and a noreply address names an account by construction.

### Who the reviewer is

`submit_verdict` takes a `reviewer`, and the RPC surface resolves it
(`Server::reviewer`) from the two sources `dispatch_origin` uses, weighed
differently:

1. the relay's verified stamp, resolved to an address — the only thing that can
   name somebody who is not sitting here;
2. this box's own session, for a local call, which is the desktop review window.

Deliberately **not** the `viaUser` a frontend claims about itself. A commit
author is provenance and takes a claim (§gh#107); a token is not something a
caller gets to spend by naming somebody. `None` — no auth service, signed out,
a roster that could not be read — posts as the board.

### What it does to the projection

`project_verdict` tries the reviewer's credential first and the board's second.
Under hers there is nothing to refuse: she did not open the pull request, so
`APPROVE` is taken as an approving review with her name on it, and no downgrade
happens at all. The receipt says whose:

```
Recorded, and delivered into the chat once. It is on the pull request, as
@bredebjorhovd.
```

Everything gh#365 built stays underneath. A member with no token reviews as the
board and gets the comment fallback. A token that has expired is logged, and the
board's own credential tries behind it — with both refusals kept, because the
second attempt was made on account of the first. The verdict itself is recorded,
standing and delivered before any of this, so none of it can cost the reviewer
their words.

### What did not move

**Commit authorship.** §gh#162 exists so a teammate's dispatched commits carry
their name; that is `GIT_AUTHOR_*` stamped on the harness child at dispatch
(`crate::git_identity`), written by the agent hours before any of this. This
decides which bearer stamps one HTTP request. `git log` says exactly what it
said before.

**`is_the_boards_own`.** It reads `POSTED_MARK` out of the body, never the
author — which is why a verdict that starts arriving under a person's login does
not quietly start being relayed back into the chat it was just delivered to. The
watermark is still the mechanism and the mark is still the backstop; the
identity was never load-bearing, and a test now pins that
(`a_verdict_cast_as_a_person_is_still_recognised_as_the_boards_own`).

### The other half is configuration, and `doctor` is where it shows

Nothing in the board *chooses* who opens a dispatched pull request: the agent
pushes and runs `gh pr create` through whichever credential the board's
credential path hands it (§gh#68). An App is a bot and the invariant holds. A
`GITHUB_TOKEN` is a person, and if that person is also a reviewer, no member
token rescues it — it is one account on both sides.

So `doctor` gained a `review identity` line that names all of it: who opens
(asking GitHub `GET /user` when the credential is a token, since only GitHub can
put a name to one), who casts verdicts under their own name, who reviews as the
board and which variable would change that. It FAILs on the collision only —
one account both sides, reached either by the opener's login matching a member's
or by the board's `GITHUB_TOKEN` being byte-identical to somebody's review
token.

### Not in this issue

**`herdr-board`'s dispatch path.** The Mac's board is a different program in a
different repository (`bredebjorhovd/herdr-board`), and it has no App credential
of any kind — the whole askpass/`gh`-shim mechanism (§gh#58, §gh#68) is
comet-board's. Its half of this issue is a port of that mechanism, and it cannot
ship in a comet-board pull request. Until it does, the Mac's board keeps opening
dispatched pull requests as its operator, and comet's `doctor` on a box in the
same shape says so.

**Branch protection.** With the split in place the board's approval becomes a
real approving review, so a rule requiring one becomes satisfiable. Nothing in
`sync.rs` gates a merge on an approval, so nothing about merging changes here.
This was always about the record being true.
