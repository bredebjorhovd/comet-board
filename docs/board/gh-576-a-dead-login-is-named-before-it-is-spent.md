# A dead login is named before it is spent — **done** (gh#576)

The agent-account slots hold live credential sets per harness, and on the
always-on box one of them expiring was discovered the worst way: a refused
dispatch or a run that died authenticating, with nothing anywhere saying
*which* login went off. The operator SSHed in, guessed at credential files,
and re-ran `claude` logins by hand until something worked.

Three pieces, all pointed at the same question — *which slot is stale, and
how do I fix it from a shell*:

- **The verdict is minted, not guessed.** An expiry stamp in the past does
  not mean a login is dead: Claude Code refreshes hourly and its refresh
  token may be perfectly fine. So freshness is answered by putting the
  stored access token to its provider — Claude's profile view, Codex's usage
  view, read-only GETs, the same calls their own CLIs render. Accepted means
  it works; 401/403 means the next run under it dies; anything else is "not
  verified", never invented into either direction. `VerifyAgentAccounts`
  (engine RPC) is where this lives, so doctor and `relogin` cannot disagree.
- **Read-only on purpose.** No refresh grant is exercised from a *check*.
  Refresh tokens are commonly single-use; a probe that rotates one is a
  write to somebody's login, and a freshness check must be safe mid-run.
- **Doctor renders one line per login** — `agent account <email>
  (<harness>)` — ok / STALE / not verified. Outside the routing match: a
  broken `routing.toml` must not hide a dead login any more than a dead
  login should hide behind a healthy config. A route naming a stale slot
  fails its own line too, naming the repair, because every dispatch it would
  release dies at first request.
- **`comet-board relogin [id-or-email]`** walks the whole flow over IPC:
  start → code → complete/poll → verify. Always the no-browser-on-the-box
  flows (`remote` forced): paste-code for Claude, device code for Codex —
  the caller is usually SSHed into the box. It reports which slot the login
  actually landed in (signing into the wrong account is a failure, said out
  loud), and only prints success once the provider accepted the fresh token.
  The engine's `StartAgentLogin` takes an explicit `remote` override now,
  because an SSH shell dials loopback like any local client but has no
  browser behind it for a callback to land in.
- **A dying run says why.** When an errored run's chat names an account
  whose stored token is past its stamp *at that moment*, the transcript leads
  with that — which account, when it ran out, `comet-board relogin <id>` —
  with the harness's own error kept behind it as evidence (§gh#533's shape:
  attribution beside the fact, never instead of it). Conservative both ways:
  no named slot, nothing claimed; a CLI that refreshed before dying leaves
  fresh tokens in its dir, the read sees them, and nothing is blamed.

The offline half of the last point is what makes it safe to say: the check
is a timestamp comparison against the slot's stored (or materialized-dir)
credentials — no probe, no rotation, no network. Attribution must never be
the thing that finishes off the login it is describing.
