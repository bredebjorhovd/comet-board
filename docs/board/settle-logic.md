# Settle logic — **done**

Landed as `crates/board/src/settled.rs` (the pure decision) plus the settle
machinery in `sync.rs` (see BOARD.md's ported-and-working list). The decision
keys off run-journal facts: `Runtime::last_run_end` reads the chat's last
journaled event (`CometRuntime` reads the engine's `RunJournal`), which is
what splits `Errored` (run ended, badly — no settle on commits, PR still
counts) from `AwaitingInput` (run alive — nothing settles) inside the one
`Blocked` status. `Idle` needs no journal read: it is only ever written
after a `Done`, and a chat is fresh per attempt. The 60-second clock is gone;
the event path settles on the status *transition*, the interval reconcile is
the catch-up. Artifact checks kept: recorded PR → commits-since-base → and,
only on the event path (the interval polls seconds earlier), one targeted
GitHub `pulls` recheck before closing on commits, which closes herdr's gh#29
window instead of wording around it.

**Commits must be on origin** (gh#69). `attempt_has_commits` is a local
`rev-list` count, so an agent that committed and could not open a pull request
— guaranteed on a headless box with no `gh` credential, which is what gh#68
went on to fix — ended `Completed`, settled on `Evidence::Commits`, and put the
row in `review` while the work sat in one worktree on one box. The crash path
had the same shape: recovery stamps an aborted run `Interrupted`, not
`Errored`, so the errored-runs-never-settle guard never covered it.
`settled::decide` now takes a three-way `Commits::{None, Unpushed, Pushed}`,
and `Unpushed` is a `StayLive` with its own `Why` — logged once per attempt,
naming the branch, and left for the §gh#70 clock to close if nobody acts. The
push check is `SyncEngine::commits_are_on_origin`: a remote-tracking ref that
*contains* HEAD (free, offline, true of any ordinary `git push`, and the only
tier a non-GitHub remote gets), then — event path only, for the same reason the
`pulls` recheck is — one `GET /repos/{repo}/branches/{branch}`, for a push made
straight to a URL, which updates no tracking ref. Containment rather than
existence, because a retry reuses its predecessor's branch. Unproven reads as
unpushed: an attempt that stays live is visible and bounded, a row that says
`review` about work nobody can fetch is the bug. A pull request short-circuits
all of it — GitHub will not open one for a branch it does not have.

`reopened` semantics kept both ways: an
`Errored`→retried run never left its attempt, and a settled attempt whose
chat works again is re-opened in place (refused when re-dispatched, closed
upstream, or marked done). The dispatcher wake (herdr AGE-25) was not ported
with this; it landed later as part of §gh#70.
