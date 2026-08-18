# Managed activation requires the complete release pair — gh#496

Tokenmaxxer's active `0.8.0-gh483-c42b8dc` directory contained `comet` but no
`comet-board`. The engine and edge rooms were healthy, while every dispatched
agent on that host was told that the board CLI did not exist. The public health
signal was therefore green for a host that could not perform its central board
workflow.

**Root cause.** Packaging had proved that release archives contained both
binaries since gh#156, but activation did not preserve that invariant.
`stage_headless` reused a directory when `comet` existed, `apply_headless`
moved `current` after the same one-file check, and the curl installer linked the
directory before checking its contents. An emergency engine-only hotfix could
therefore bypass the archive proof. Headless boot then logged a warning about
the absent CLI on a detached thread and continued serving as healthy.

**One runtime invariant.** A managed release is a real directory containing
regular, executable `comet` and `comet-board` files. Both must answer
`--version` within five seconds and both answers must exactly match the release
being staged or run. `comet` now exposes the same clap version surface the board
CLI already had. Symlinked payload directories or binaries are refused rather
than followed outside the version directory.

**Before the point of no return.** The native updater validates that invariant
when reusing an existing stage, after unpacking a downloaded archive, after a
concurrent stager wins, and immediately before moving `current`. The Linux
installer unpacks into a same-filesystem temporary directory, validates it,
then publishes the directory and replaces `current` by rename. A missing,
mismatched, or non-executable sibling leaves the previous target untouched.

**Boot is the final fence.** A managed `comet headless` validates the exact
directory containing its running executable before constructing the engine.
Manual or emergency activation that bypasses both supported installers is
therefore a loud service-start failure, not a reachable device whose agents
discover the missing CLI hours later. Managed board-executable resolution is
then pinned to that validated sibling, so an environment override cannot undo
the proof. Unmanaged source builds retain their existing behavior.

**Regression boundary.** Unit fixtures cover an engine-only payload, a version
mismatch, and a non-executable sibling. Every refused activation asserts that
`current` still names the previous complete release. A Linux test drives the
real curl installer with an engine-only tarball and proves the same unchanged
symlink. The original engine-only activation test failed before the repair and
passes only after the activation fence was added.

Closes #496.
