# The frontends send an account, and say who dispatched — **done** (gh#74)

`DispatchTask` has taken an `account` since gh#59 and no frontend sent one, so
a dispatch from the panel spent whatever the route said — which on a shared box
means the owner's subscription, whoever pressed enter. Nothing recorded who that
was either: `Dispatcher::Operator` is anonymous by construction.

**The account picker.** The desktop panel's dispatch picker grew a third strip
between runtime and model; the TUI, which had no picker at all, opens one on
`enter` over a ready row. Both are fed by `ListAgentAccounts` **on the board's
host** — the run executes there, and a slot id means nothing on the device that
did not save it, the same reason `ListModels` is fetched with the host
passthrough. Both filter the slots to the harness the row's runtime resolves to,
which is why `ListBoardRuntimes` now carries `harness`: a Claude slot cannot pay
for a codex run (`CLAUDE_CONFIG_DIR` and `CODEX_HOME` are not interchangeable),
and offering one would be offering a dispatch that refuses itself.

Row 0 in both is **the route's own account**, and sends no override — so
enter-enter is exactly what enter did before, and the strip costs a keystroke
rather than a decision. It names the route's account where the row knows one, so
the default is a fact rather than a shrug. Picking a slot is one click and never
itself the release: whose limits a run burns is too consequential to happen by
accident, so the model row (or enter) still does the releasing.

**Attribution, at the strength the transport allows.** Every dispatch from
either frontend now carries `viaDevice` (this device's id) and `viaUser` (the
signed-in email), recorded on the attempt as `dispatched_by_device` /
`dispatched_by_user`. `dispatched_by_user` joins the `TaskRow` contract, so
`list --json` and both viewports can say who released a row; the device id
deliberately stays off the wire, since it names a laptop, not a person. With no
agent in the chain the upstream dispatch comment names the human — "dispatched
by ana@example.com" — where it previously said nothing at all.

These are **claims, not credentials**, and `DispatchOrigin` says so where the
code is: relayed board calls arrive as the device room's owner (§gh#55), so the box
has no per-call identity to check them against. #66 established that a teammate
may reach the box at all; establishing *which* teammate is the next step, and
these two columns are where a verified identity will land. Until then nothing is
authorized on them, and in particular no account is inferred from them — which
subscription a run spends stays the explicit `account` (gh#59).

*(§gh#161 landed that verified identity: `dispatched_by_user` is now the edge's
answer for a relayed dispatch and the frontend's only for a local one, with
`dispatched_by_verified` saying which. Nothing below changed — it is still not
authority, and no account is inferred from it.)*

Deliberately not here: a per-user default account (that is a preference, and
preferences want a home and a settings surface), and any UI for reading the
attribution back beyond the row field — the panel's dispatch notice names the
account it spent, and the issue comment names the human, which is where people
were already looking.
