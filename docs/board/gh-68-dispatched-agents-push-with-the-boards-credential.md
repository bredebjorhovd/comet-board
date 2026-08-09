# Dispatched agents push with the board's credential — **done** (gh#68)

#58 built the askpass machinery and left it with no caller, so a dispatched
agent pushed with whatever git credentials the box user had: fine on a Mac
somebody set up by hand, nothing at all on a clean headless box. This is the
caller, threaded through the same harness-env seam `COMET_BOARD_CHAT_ID` and
`CLAUDE_CONFIG_DIR` already use (`RunControls.push` →
`comet_harness::PushCredentials::apply`).

The repo is resolved at dispatch (`DispatchSpec.push_repo`: the task id for a
GitHub ticket, the checkout's `origin` remote for anything else) and stored on
the chat as `ChatConfig.push_repo` — on the chat rather than the run for the
same reason `account` is, since the fix for a review comment next week is a new
run in the same chat and has to reach the same branch. `crates/engine/
push_credentials.rs` turns that repo into an environment, per run.

**Late minting, twice.** `git push` goes through askpass, which mints inside the
push. `gh` has no askpass — it reads `GH_TOKEN` once, at startup — and an
installation token lives an hour while a run does not, so exporting one at spawn
would hand a three-hour run an expired credential exactly when it goes to open
its pull request. Instead a generated `gh` wrapper goes on the front of the
child's PATH and mints per invocation (`comet-board gh-token`, the `gh` twin of
`git-askpass`). The token reaches that one `gh` process's environment, which is
gh's only interface; it is never in the agent's own.

**Scoping.** One repo per run, and it is the attempt's. Because the helper now
answers for every `git` the agent runs rather than for one push the board
issued, `askpass` refuses any prompt naming a host other than github.com — an
installation token answered to a `git fetch https://gitlab.com/…` would be a
credential handed to a stranger. The wrapper defers to a `GH_TOKEN`/
`GITHUB_TOKEN` the operator set and to a non-github.com `GH_HOST`, for the same
reason.

**Every part is optional and fails back to what happened before.** No board
credential, no `comet-board` binary, no `gh`, no repo on the chat: the child is
spawned untouched and the agent pushes as the box user. The PAT path is
unchanged — `token_for_push` hands back the static token, which is what a
self-hosted board on a PAT already pushes with. `comet-board doctor` answers the
question directly with a `dispatched pushes` check.
