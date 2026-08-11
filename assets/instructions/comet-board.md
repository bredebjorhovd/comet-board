## You may be running under the comet board

This machine runs comet-board: one queue across every space, GitHub and Linear
issues in, coding agents in comet chats out. If your first message named a task
and a branch, the board dispatched you and everything below is about you. If it
did not, this is a fact about the machine you are on and nothing more.

`comet-board` is on your PATH — the copy the engine shipped with. Read the board
before acting on it, and never read `board.db` directly: the schema moves, the
CLI shape does not.

**Commit and push before you stop.** The board has no callback; it decides an
attempt is finished by seeing an open pull request, or commits *on origin* for
the attempt's branch. Work left uncommitted — or committed and never pushed —
reads as an agent that is still going, and the row sits in `working` until the
clock cap takes it or a human notices. Push even when you are not opening a
pull request.

**The credential for that push is the board's, and it is the only one.** Your
run gets `GIT_ASKPASS` pointed at a helper that mints a short-lived token onto
git's own pipe, and a `gh` on PATH that does the same per invocation — so no
token is ever in argv, in `.git/config`, or in your environment, on a box
several people share. If a push cannot authenticate, **say so and stop**: do
not write a credential wrapper of your own, do not export a token you found, do
not put one in a remote URL. The board records whether its credential was the
one that pushed and comments on the issue when it was not, so a push that got
through some other way is a finding, not a finish.

**Say what you changed, in claims a reviewer can check.** `comet-board claim
--task <id>`, one line each, `<what you did> :: <anchor> [<anchor>…]` — an
anchor is a repo-relative path or a symbol. What comes back is the part worth
reading: every changed file no claim accounts for, computed from the branch diff
rather than from what you wrote, which is where the dependency you bumped and
the function you edited in passing turn up.

**Work you delegate goes through the board.** A ticket buys a branch, a pull
request, a review that reaches the agent that wrote it, a cap, and a bill with a
name on it; your own harness's in-chat subagents buy none of that — agents
editing a real repo with no row and no presence in any frontend, so nobody can
see they are running. Subagents are for reading. Anything that lands a commit is
a ticket, and no ticket is ever dispatched speculatively: releasing work starts
a real agent in a real repo that commits and opens pull requests, and a human
keypress or an explicit instruction is what does that.

**This block is about the board and nothing else.** How to write code in the
checkout you are standing in is the business of that repo's own `AGENTS.md` or
`CLAUDE.md`, and nothing here overrides them. Where they appear to disagree with
this, the repo wins — it is the one that knows what it is.

{{REFERENCE}}
