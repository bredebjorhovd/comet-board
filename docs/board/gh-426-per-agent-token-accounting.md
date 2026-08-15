# Per-agent token accounting with cost estimates — **done** (gh#426)

gh#151 preserved one authoritative four-bucket token total per attempt, and
gh#182 priced that total at the model recorded for the run. That fallback is
still honest for old journals, but it cannot price a turn where Claude's main
agent used one model and delegated research to another. This ticket retains the
more exact evidence the Claude stream already carries, without changing the
meaning of the old total.

- **One total, two attribution views.** `AgentEvent::Usage` remains the only
  event added into an attempt's authoritative token total. Claude result frames
  additionally emit `ModelUsage`, and complete assistant messages emit
  `AgentUsage`. The journal scanner holds those views until the corresponding
  result-level `Usage` arrives and accepts only a four-bucket exact match, so a
  cancelled resumed turn cannot add partial attribution to an older total.
- **Claude message ids are the accounting boundary.** The CLI may repeat a
  complete assistant frame for one API step. Its stable message id is deduped
  before an agent row is emitted. A null `parent_tool_use_id` means the main
  agent; a populated id means delegated work, and the Task/Agent launch is
  remembered so the row can say `Explore`, `Plan`, or the harness name rather
  than only `Subagent`.
- **Mixed-model pricing is accepted only when it reconciles.** The result
  frame's `modelUsage` map is used when its four buckets add up exactly to the
  authoritative total. If a CLI version omits it, or a partial result does not
  reconcile, stats keep gh#182's attempt-model fallback instead of silently
  losing tokens. Unknown models remain unpriced, never zero.
- **Nullable all the way down.** `token_models` and `token_agents` are nullable
  JSON columns beside the existing token buckets. NULL means the journal did
  not expose attribution. Old attempts and other harnesses stay blank; they are
  never rewritten as an empty main-agent row.
- **Coverage travels with both numbers.** The total list-price API estimate is
  qualified by token reporting coverage. The agent/model section separately
  says how many token-reporting attempts exposed an exact whole-attempt agent
  split. Desktop, iOS, and the CLI use the same `BoardStats` fields. JSON keeps
  the compatible `listPrice` and `cost` names, while the response-level
  `pricingBasis: listPriceApiEstimate` discriminator states that every one is
  an estimate rather than a bill; the new agent-row money field is also named
  `listPriceApiEstimate`.
- **An estimate, never a bill.** Every rendered dollar total or row is labelled
  a list-price API estimate and the cards state that subscription runs do not
  pay per token. Plan amounts remain a separate user-entered fact and are never
  added to the estimate.

There is deliberately no token or cost cap here. A future cap may consume the
same attempt facts, but it must define its own enforcement boundary rather than
turning an observational estimate into a billing claim.
