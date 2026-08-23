# Board pull requests announce closure in prose GitHub does not understand — **done** (gh#548)

Found on `Florin-AS/Tally` (2026-08-20), and it is the convention rather than
an accident. Tally PR #967 opened its body with **Lukker gh#932.** — Norwegian,
matching the repo's language, which is right for the prose and still nothing to
GitHub:

- closing keywords are English only: `close/closes/closed`, `fix/fixes/fixed`,
  `resolve/resolves/resolved`;
- `gh#932` is not a reference GitHub parses. `#932`, `owner/repo#932`,
  `GH-932` and a full issue URL are; `gh#932` is not.

So GitHub created no linked-issue relation at all: nothing under *Development*,
an empty `closingIssuesReferences`, a merge that closed nothing. It went
unnoticed because `writeback = true` closes the issue on settle — the loop
looks whole, but it is the board doing it, not GitHub, and every native
consequence of the link was silently absent. Anything downstream that reads
GitHub's link sees nothing: that is how it surfaced, when Mylder's return leg
read closing keywords to learn a request exists for a story and got
`ignored: "pull request action"` — on that repo it could never have worked.

Orion PR #50 failed in the other direction: no closing keyword anywhere. Both
spellings are instruction drift, which is why this lands as an ask **and** a
check.

### The ask

`crate::closing::brief` is appended by `resolve_prompt` for every task whose id
names a GitHub issue, beside the base line (`pr_base_line`) because both are
about the pull request the agent is about to open and both are facts about the
dispatch a route author cannot know when they write their template. It names
the exact line — `` `Closes #<n>` `` on its own line, with `<n>` taken off the
task *id*, because `Closes gh#932` would be the Tally failure wearing a
keyword — and names the failure spellings so an agent can recognise its own
draft. The rest of the body stays in whatever language the repo writes in; the
two are not in tension. Linear rows get nothing: there is no GitHub issue for
`Closes` to name.

### The check

The board verifies at first sight rather than only asking.
`SyncEngine::link_pull_requests` already holds the body (the pull list has
carried it all along) and the issue number, so the first poll that attaches an
open request to its task runs `closing::parses_as_closing(body, n)` and warns
into the log with the repair in it when nothing there would make GitHub act.
The check accepts what GitHub accepts — keywords anywhere in prose, case- and
punctuation-tolerant, `#n` / `GH-n` / `owner/repo#n` / full URL — not what the
brief asks for: the contract is GitHub's parser, and "own line" is how an agent
reliably produces parseable text, not the definition.

### What is deliberately not here

**No shim opinions about argv.** The board does not open the pull request —
the agent's `gh pr create` does — and growing the `gh` shim into a body
editor was rejected for `--base` already (see `pr_base`'s comment): argv
surgery to cover what one sentence states plainly.

**No gate, no DB column.** A missing reference is not a failed attempt; the
work exists, and writeback still closes the loop on settle. The warn-once log
line is the operator-facing surface until something downstream needs more.

**Warned once per request**, at first sight, not every poll — a warning that
repeats each cycle is noise an operator filters out. A body fixed later is
honoured silently by the same check.

### Where to watch for it working

The next board-dispatched pull request on a GitHub issue: its page shows the
issue under *Development*, and merging it closes the issue without the board
settling anything. On the box itself, `state/syncd.log` carries the warning for
any request already open with an unparseable body, exactly once.
