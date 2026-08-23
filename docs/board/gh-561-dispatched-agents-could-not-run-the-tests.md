# The agents could not run the tests — **done** (gh#561)

Read off two consecutive dispatched attempts, both touching `edge/`, both
opening pull requests whose tests they had never executed: gh#553
(`Tests 2606 -> 2609, never run`) and gh#557 (`Tests 2609 -> 2618, never run`,
`nothing that verifies anything ran, across 59 commands`). gh#557's cost a full
CI cycle to catch a heading-level failure a 0.25s test would have caught before
the push.

### The cause was a PATH fact, not a capability fact

The obvious reading — "the workerd tier cannot run in a dispatch" — is wrong,
and the gh#557 agent said so when asked:

> `node` and `npm` are **not on a dispatched agent's PATH** on the box, and
> `edge/node_modules` does not survive between sessions. Both suites run fine
> once that is supplied.

Supplied by hand it then ran 101 unit + 35 workerd tests green, plus a clean
typecheck. The distinction matters: a tier that cannot run makes "never run" an
honest limitation; a PATH gap makes every such review read as an agent that
skipped its verification when it was an agent with no way to do it. The runs'
own commands show the workaround being rediscovered from scratch each time
(`PATH=/opt/homebrew/bin:$PATH npm test`) — a tax every noticing agent paid,
and the ones that did not notice shipped unverified.

Three causes, all environmental:

1. **The engine's process PATH is not a login shell's.** A GUI or systemd
   launch inherits `/usr/bin:/bin` and nothing shaped by any shell, so
   Homebrew's `/opt/homebrew/bin` — where this box's node lives — is absent.
   The harness adapters already stamped *some* directories onto children, but
   only those holding the harness CLIs themselves.
2. **A fresh worktree has no `node_modules`.** Worktrees share git objects,
   not untracked build trees, so even with node on PATH the first `npm test`
   fails with missing-module errors about the empty worktree, not the change.
3. **Nothing surfaced any of this beforehand.** `doctor`'s `agent PATH` check
   answers "can an agent find `comet-board`" (gh#184's question), not "can it
   run what the routed repos need"; and the board computed "tests added, never
   run" on both attempts without either review leading with it.

### 1. Toolchain directories ride behind every child's PATH

`comet_board::toolchain::agent_tool_dirs()` is now the single source of truth
for the install locations a human shell ends up with and a GUI/systemd launch
does not: `/opt/homebrew/bin`, `/usr/local/bin`, `~/.local/bin`,
`~/.cargo/bin`, and the Node version managers' bin dirs (fnm's stable
`aliases/default`, nvm's installed versions, volta/bun/pnpm shims). Only
directories that exist are returned.

The engine resolves the list once at assembly (a property of the box, like
gh#184's `agent_bin_dirs`) and hands it to every harness child through the new
`RunControls::tool_dirs`. Each adapter applies it with a new
`append_missing_to_path` helper — **appended after** the inherited PATH rather
than prepended: these are gap-fillers, and putting them behind everything means
they can never shadow something the engine's launcher already resolved, while
dedupe keeps a long-lived engine from growing PATH on every run. Codex's forced
push-environment PATH gets the same tail, so the attested environment and the
ordinary one stay one story.

### 2. The brief names the missing dependencies

`DispatchSpec::prompt_at` runs at the one moment the checkout exists but no
command has failed yet, so it is where the note belongs. A bounded shallow walk
(`crates/board/src/toolchain::js_packages`, depth two, skipping
`node_modules`/`target`/`dist`/…) finds every declared JS package whose
`node_modules` is absent and appends one paragraph: which directories, which
install command (`npm ci` under a package-lock, `yarn install`, `pnpm
install`), and why — so the agent installs before verifying instead of
diagnosing phantom breakage. A warmed worktree or a space-folder route gets
nothing.

Auto-running the install at checkout time was considered and declined: a
dispatch would block on a network-heavy package manager before its agent could
even read the ticket, and the failure mode when the registry is slow is worse
than a paragraph.

### 3. `doctor` asks the toolchain question

A new **repo toolchains** check sits beside **agent PATH** (which keeps its
old job): it takes every local routed checkout, detects the tools it needs from
markers ([`repo_tools`] — `package.json` → node+npm, `Cargo.toml`/
`rust-toolchain.toml` → cargo), and resolves them over exactly the PATH an
agent is guaranteed — board dir plus the gap-fillers. What the engine's
launcher happened to put on PATH is deliberately *not* counted: from a doctor
process it is invisible, and a check that passes in the operator's shell and
fails in dispatch is precisely the lie gh#184 ended. A miss names the tool, the
repos it blocks, and what shipping anyway costs.

### 4. "Tests added but never run" is now a finding

The effects chip said `never run` on both attempts and neither PR mentioned it,
so the suffix became a sentence: [`FindingKind::TestsNeverRun`] fires (alarm
tone, verdict-loud) whenever a branch adds tests and no test command ran —
distinct from `Unchecked`, which needs commands that checked nothing and so did
not say what the busy run missed. The iOS vocabulary pins the new kind as
`tests_never_run`.

### What this does not claim

Routes still cannot declare arbitrary toolchains, and detection is markers-only:
cargo and the three JS ecosystems cover what this workspace routes today. A
route-level `env`/prep key remains the right shape if a repo ever needs
something marker-invisible — worth doing when one exists, per the same rule
that kept `--base` out of the gh shim.
