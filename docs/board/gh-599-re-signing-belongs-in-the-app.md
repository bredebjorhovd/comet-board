# Re-signing belongs in the app — **done** (gh#599)

Brede on gh#576's flow: "I don't want to do that in the terminal." The SSH-friendly
`comet-board relogin` verb solved headless access, but the natural place to re-sign a
slot is the surface that shows the slot in the first place — Settings → Agents, which
already lists every login and can add one. What it lacked was freshness and renewal.
Three pieces:

- **The verdict is worn, not hunted.** Every account row carries a freshness badge —
  Verified / Stale / Not verified — minted by the same engine RPC doctor quotes
  (`VerifyAgentAccounts`, gh#576). No badge until the probe has answered: absence is
  "not checked", never a claim of health, and a fetch that fails clears what was held
  rather than leaving a verdict the engine no longer stands behind. A non-fine verdict
  also speaks its sentence under the title — what was asked of which provider, what
  came back — in the blocked tone when refused, as a quiet notice when merely
  unverifiable.
- **Re-sign drives the flow already there.** The same START_AGENT_LOGIN walk the Add
  account button uses — paste-code URL or device code in-window, the code back in a
  field, live progress — but tracked, so the ending can say what actually happened.
  The dialog does not close on transport success: the page reloads the accounts,
  resolves which slot's snapshot moved past a pre-login watermark, puts that slot's
  stored credential to its provider, and only then renders the verdict
  (`Verifying` → `Settled`). Success exists only where the provider accepted AND the
  slot is the one asked about; signing into the wrong account is named out loud —
  who actually moved, that the asked-for slot still holds its old login — exactly
  what the CLI verb prints, carried over whole. The switcher's passthrough follows:
  re-signing another device's slot starts the no-browser flows over there and
  verifies over there too.
- **Every hint points at both surfaces, weighted for where it renders.** Doctor's two
  stale-slot lines name Settings → Agents first and keep `comet-board relogin <arg>`
  beside them; the dying-run transcript line — which renders *inside* the app, the
  one place the surfaces genuinely overlap — points at the app's Re-sign action
  first and keeps the shell verb for SSH-only moments.

Desktop first; the flow survives small screens only if it earns it.
