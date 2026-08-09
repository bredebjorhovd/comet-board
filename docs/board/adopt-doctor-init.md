# Adopt, doctor, `init` — **done**

Landed as `crates/board/src/{adopt,doctor,init}.rs` plus `apps/board-cli`
(binary `comet-board` — §board-cli's binary, started early with the three commands
that need only §board-service):
- `doctor` — herdr checks replaced with comet ones: per route the *space*
  exists (case-insensitive display-name match), the repo is a git checkout,
  the runtime resolves to a comet harness; plus "engine reachable on the IPC
  port" instead of pidfile-based `syncd` liveness. The herdr-only checks
  (manifest overrides, stall nudge) are gone. An unreachable engine fails its
  own check and leaves route space-checks "not checked" rather than failing
  them all.
- `init` — walks this device's spaces (first `WatchSpaces` snapshot, filtered
  by `LocalDevice`); `git_detected` gates, linked worktrees are skipped as
  attempts' checkouts. Linear team discovery unchanged.
- `adopt` — detection offers git-detected spaces whose repo is missing a
  route and/or a `[github] repos` entry; the validated text-edit writer,
  `.bak` backup, ignore list, and backlog preview came over verbatim. The
  label-picker survives as `--labels`/`--all-issues` on the CLI. §gh#71 took that
  reuse: `WriteBoardConfig {op: adopt}` calls `adopt_with` unchanged, and the
  settings page's Add is that call. §gh#96 took it again, from the other end: what
  `adopt` offers is repos with a checkout *already on the box*, and `onboard`
  (gh#97) is the same writer reached from a repo the box has never seen.
