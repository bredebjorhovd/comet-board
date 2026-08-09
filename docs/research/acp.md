# ACP: should comet-board convert to one harness for every agent? (gh#147)

Evaluation of upstream's `ca05336` "Convert claude/codex to ACP: one harness for
every agent (#28)", following `c951c3e` "ACP harness: Grok Build, slash commands,
tool output + inline diffs (#24)". Upstream's own record is
`docs/research/acp.md` on `upstream/main`; this is ours, and it disagrees with
theirs in two places on purpose.

## Decision

**Adopt, additively, and not yet as a replacement.** Take the ACP harness as a
*fourth* adapter beside `claude`, `codex`, `opencode` — not as the thing that
deletes them — and let it carry the agents we do not have today. Convert the
existing three only once an ACP-backed Claude has run a full board dispatch,
including a push and a pull request, against a per-account config dir.

The reason is not doubt about ACP. Both load-bearing questions came back
**yes**, and they were measured, not reasoned about (below). The reason is that
our fork's weight is not in the adapters, and a wholesale swap spends its risk
in the one place a conversion gets no help from upstream: everything downstream
of `HarnessId`, which upstream does not have.

What this pass actually landed: the two measurements as a runnable probe, and
the one file a conversion needs on day one moved to where upstream keeps it.

## What was measured

`crates/harness/tests/acp_probe.rs` — two `#[ignore]` tests against the real
org adapters (`claude-agent-acp@0.66.0`, `codex-acp@1.1.14`) over our own
JSON-RPC client. Both pass as of 2026-08-09:

```
cargo test -p comet-harness --test acp_probe -- --ignored --nocapture
```

### 1. The account slots survive the extra process hop — yes

gh#59 works by pointing a child at a config dir of its own and nothing else.
Under ACP the child is the *adapter*, one process further out than the CLI that
reads the variable. It carries:

- `CLAUDE_CONFIG_DIR=<scratch>` → the adapter materialized `.claude.json`,
  `sessions/`, `backups/` **into that dir**, not into `~/.claude`.
- `CODEX_HOME=<scratch>` → `session/new` came back
  `-32000 "Authentication required"`, because a fresh home holds no login.

The second is the stronger result, and it is an *improvement*: today a
signed-out slot surfaces as a failed turn well after a worktree has been cut,
where ACP refuses at session creation with a typed error. `AgentAccount::apply`
keeps working unchanged — same variables, same dirs — and so do
`AgentAccounts::signed_in` and `runtimes::availability` (gh#187), which read
`.claude.json`'s `oauthAccount` and `.credentials.json` inside that same dir.
The adapter writes the file we already look for.

### 2. Billing survives — yes, but not through upstream's mapping

The board prices attempts per account (gh#151, `comet_board::prices`) out of
`TokenUsage`'s four disjoint buckets, and it is the only consumer of them in
either fork. The settled `session/prompt` response carries all four:

| | `inputTokens` | `outputTokens` | `cachedReadTokens` | `cachedWriteTokens` |
|---|---|---|---|---|
| claude-agent-acp | 2 | 4 | 15 273 | 15 577 |
| codex-acp | 11 555 | 5 | 11 008 | — (never written) |

They are disjoint — each row sums to the adapter's own `totalTokens` — which is
the property `TokenUsage::total` already assumes, so the mapping is a straight
copy with nothing derived. codex additionally offers a per-model breakdown under
`_meta.quota.model_usage` that our app-server path has no equivalent for.

**Upstream's `usage_from_response` reads `inputTokens` and `outputTokens`
only.** On the claude turn above that is 6 of 30 856 tokens — it would report
0.02% of what the turn spent, and the board would price a month of Claude
dispatches at roughly nothing. This is a lossy *reading* of a lossless wire, so
it costs about four lines to fix in a port; it is on this page because taking
`acp/mod.rs` verbatim is exactly how it would be missed. Upstream has no
billing, so nothing there would ever notice.

## What ACP does not buy us

**"A new agent becomes configuration" is not true, and upstream is the proof.**
`HarnessId` is still a closed enum on `upstream/main` — Grok, Hermes and Pi are
each a new variant plus a registry block plus an `AcpAgentSpec`. What the
conversion actually changed is the *size* of a port: ~1 800 lines of bespoke
crate became ~40 lines of spec. That is a large, real win. It is not
configuration, and on our side each new agent additionally costs
`harness_for_runtime`, `RUNTIME_NAMES`, `runtime_name`, `runtime_options`,
`signed_in`, `locate_cli`, and a doctor row — none of which exist upstream.
57 files in this tree name `HarnessId`.

So `runtime_options()` becomes: *the same function, with more rows*. It stays a
board-side constant listing spellable names, `RuntimeOption.harness` keeps
pointing at a variant, and `comet_engine::runtimes` keeps stamping availability
per device. Nothing about it is blocked by ACP — and nothing about it is solved
by ACP either.

**Our mock is not at risk, and does not need hosting.** The question was
whether ACP can host it; the answer is that it does not have to. Upstream kept
`mock.rs` as a native `Harness` impl through the whole conversion — the trait
survives, ACP is one implementation of it. `demo` and the integration tests keep
releasing real tasks through a `HarnessId::Mock` that never spawns anything.
Had ACP demanded a subprocess for every agent, this would have been the finding
that sank it.

**`locate_cli` keeps its meaning.** ACP splits "installed" in two — the adapter
(fetchable on demand via `npx -y`) and the agent's own CLI. Upstream models this
as `executable` vs `cli_executable`, and only the latter is what a user means.
`HarnessCli::Found/Missing` keeps probing `claude` and `codex`; only the spawn
target changes.

## The costs a conversion has to buy, in order of how much they hurt

1. **Resume ids do not transfer.** The claude adapter keeps sessions in
   `<CLAUDE_CONFIG_DIR>/sessions/`; the CLI keeps them in
   `<CLAUDE_CONFIG_DIR>/projects/<escaped-cwd>/*.jsonl`. Different stores, so
   every `session_id` our docs hold today is unloadable after a cutover and
   falls back to a fresh session. The doc keeps the transcript; the *agent*
   forgets. This is a dateable, one-time cost, and it argues for converting on a
   quiet board rather than mid-flight.
2. **Everything in `RunControls` past `interrupt` has to be re-hung.**
   Upstream's `RunControls` is three fields. Ours carries `chat_id`, `account`,
   `push` and `bin_dirs` — the board's whole provenance and credential story
   (gh#59, gh#68, gh#184) — applied per adapter at spawn, in a PATH order that
   three tests in `lib.rs` pin. In ACP that is *one* insertion point
   (`spawn_agent`) instead of three, so this gets simpler, but it is not
   inherited from upstream and the ordering rule has to survive the move.
3. **The codex worktree-sandbox escalation is ours alone.** `codex/mod.rs`
   escalates `WorkspaceWrite` → `DangerFullAccess` for a linked worktree on a
   slash-named branch, because codex ≤0.144.x derives a malformed mount and
   nothing runs. Every board dispatch is a linked worktree on a `board/…`
   branch — i.e. this fires on *all* of them. Upstream calls sandbox policy
   "adapter-owned" and drops it. We cannot: we would be converting into a
   configuration where every dispatched codex run dies before its first command.
   Whether `codex-acp` still trips the same derivation is **not yet measured**
   and is the next probe to write.
4. **opencode has no upstream path.** ~2 000 lines that upstream never had and
   will never carry. It stays bespoke either way, which also means the
   `Harness` trait keeps earning its keep.

## Staged plan

- **Stage 0 — done here.** `codex/rpc.rs` → `crates/harness/src/jsonrpc.rs`,
  public, with codex repointed at it. It is byte-for-byte the client an ACP
  adapter needs, and it is the file upstream promoted first; matching its name
  and place removes it from the conflict surface of every later stage. Plus the
  probe above, so these findings can be re-taken against a newer adapter instead
  of re-argued.
- **Stage 1.** Port `acp/` behind a new `HarnessId`, carrying no existing agent
  — the shared harness lands with something that has no bespoke adapter to
  regress. Fix the usage mapping to four buckets on the way in. Re-hang
  `RunControls` at `spawn_agent`.
- **Stage 2.** `AcpHarness::claude()` beside `ClaudeHarness`, chosen by env
  override, and run a real board dispatch through it end to end: per-account
  config dir, a push, a pull request, a settle. Nothing converts until that is
  green.
- **Stage 3.** Codex, once the worktree-sandbox question in (3) is answered.
  Delete the bespoke adapters only after both have run dispatches for a week.

## On the timing

Upstream is at v0.1.27 and shipping daily; we forked at v0.1.6 and are 114
commits out against their 91. The "gets more expensive the longer we wait"
framing does not really hold for us — we are long past the point where any of
this merges, and every stage above is a hand-port regardless. What *does* hold
is the other half: three agents (Grok, Hermes, Pi) have been added on top of
`acp/` since the conversion without touching it, which is the strongest evidence
available that the shared harness is load-bearing rather than a refactor that
happened to compile.

## Citations

`ca05336`, `c951c3e`, `upstream/main:docs/research/acp.md`,
`upstream/main:crates/harness/src/{acp/mod.rs,lib.rs,jsonrpc.rs}`,
`upstream/main:crates/engine/src/registry.rs`; live `initialize` /
`session/new` / `session/prompt` handshakes against
`@agentclientprotocol/claude-agent-acp@0.66.0` and
`@agentclientprotocol/codex-acp@1.1.14` (2026-08-09, recorded in
`crates/harness/tests/acp_probe.rs`).
