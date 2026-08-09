# Build output is a cache, not evidence — **done** (gh#186)

The box hit 76% of a 150 GB disk on 2026-08-09 with eight checkouts, and
`doctor` failed on it: `8 checkout(s), 109.5 GiB in ~/.comet-native/worktrees`.
Measured per worktree, `board-gh-161-comet-board` was 36 GB — and 298 MB of that
was the checkout. **A checkout is 14 MB; its build output is 20–36 GB.**
`retain_worktrees` governed both, so the cheap thing and the expensive thing
were kept for the same week. Clearing `target/` out of the three whose pull
requests had already merged took the box from 36 GB free to 123 GB.

- **The two are kept for different reasons, so they get different clocks.**
  §gh#72 is right about the checkout: an attempt in review must keep its working
  directory, because review delivery resumes an agent *in that directory*, and
  after the merge it stays a while so a human can see what the agent actually
  did. At 14 MB a week of that is free. The build output has no such reason —
  nothing reads `target/` once the run ends. `[defaults] retain_build_output`
  (**`on-settle`**, `off` honored) is swept by `SyncEngine::sweep_build_output`
  beside the other two, and `gc::cache_standing` consults exactly one fact:
  whether anybody is building in there. An open pull request, an issue still
  owed, a task that has not left the board — all hold the checkout and none of
  them hold the cache. A review comment that restarts the agent rebuilds,
  slower; the alternative is paying 36 GB per attempt for the chance of saving
  one `cargo build`.
- **A per-language list, because guessing by size is not honest.**
  `gc::BUILD_OUTPUT_DIRS` is `target`, `node_modules`, `.next`, `.turbo` —
  `target/` is the Rust case, and `node_modules/` is the same shape for the JS
  repos this box also routes. "Delete the biggest directory" would be right
  about `target/` and wrong, silently, about the repo that keeps a dataset in
  the tree. `dist`/`build` are deliberately out (some repos commit them), and
  so is `.venv` (an environment somebody may be *in*). The walk never enters
  `.git`, never follows a symlink — `node_modules` is full of them, and
  `remove_dir_all` on a link would take the target's tree — and stops at a
  depth bound `doctor`'s measurement shares, so what the report calls
  regenerable is exactly what the sweep would take.
- **A sweep is not a collection**, and the columns say so:
  `attempts.cache_sweepable_at` / `cache_swept_at`, never `collected_at`. The
  checkout is still there, still on its branch, still the directory delivery
  would resume an agent in; recording a sweep as a collection would have the
  board reporting space it had not reclaimed and then never reclaiming it. It
  is also the one leaving that comes *back* — a re-opened attempt builds again,
  so `rewatch_settled_attempts` clears both stamps and the next end sweeps the
  new cache.
- **`doctor` reports the split.** The `worktrees` line names the total and both
  halves (`9.0 KiB in … (1.0 KiB of checkout, 8.0 KiB of build output)`), and a
  new `build output` line carries the cache's own weight, how many directories,
  how many tracked checkouts have been swept, and its own window. The checkout
  verdict is now measured on `Usage::checkout_bytes`, so a box mid-build does
  not fail a report for working; the build-output line is red in exactly one
  state — a lot of it with `retain_build_output = off`, which is the gh#186
  failure itself. `109.5 GiB in worktrees` was true and useless: it hid that
  99.96% of the number was regenerable, and it named the key that governs the
  other 0.04%.
- **The agents are told**, in `docs/agent-conventions.md`: your checkout keeps
  everything you wrote, your build output does not survive the end of your run,
  so a resumed attempt's first build is a cold one.
