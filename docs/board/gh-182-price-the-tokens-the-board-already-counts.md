# Price the tokens the board already counts — **done** (gh#182)

§gh#151 made the board count tokens and closed with the sentence this ticket
starts from: *"pricing is a separate ticket… whatever is shown must be labelled
a list-price estimate of the same usage on the API, not a bill."* This is the
backend half of §gh#179 — that ticket redesigns the Stats page around spend,
this one makes spend a number the page can render, in `crates/board` and
`crates/proto` so the two can run in parallel.

**Two facts, and the whole design is keeping them apart.**

- **List price is per model**, and it is the board's own number: Σ over
  `tokens_by_model` of that model's four rates. It answers *what did this cost
  at the meter*.
- **Subscription cost is per account**, and it is one person's: what the
  operator pays for the plan a slot spends. It answers *how far does that
  carry it*.

They live in different config tables, they are never summed, and only the
comparison between them is the headline anybody wanted. A single `cost` field
would have been convenient right up until a second teammate's slot landed on
the box (§gh#59), at which point it would be adding up other people's bills and
calling the total the board's spend.

- **Four rates per model, or the feature is decorative.** `TokenUsage`'s four
  buckets are disjoint and priced nowhere near each other: a cache read is a
  tenth of fresh input, a cache write a quarter more than it. A coding agent
  replaying a long context every turn is overwhelmingly cache reads, so a single
  $/token rate would not be a rounding error — a test in
  `crates/proto/src/view/rates.rs` prices one realistic turn both ways and pins
  the gap at **more than 5×**. `ModelRate::published` takes the two numbers a
  price list publishes and derives the cache pair at the published multipliers,
  which is also what an override may do — two numbers cannot be got wrong by
  fat-fingering a `0.1`.
- **A dated table shipped with the binary, not a scheduled lookup.** Neither
  provider publishes a pricing API; pricing lives on a docs page and the
  programmatic sources are third-party aggregators. Scraping HTML for money — on
  a box with no network guarantees, into a number nobody re-checks — fails
  silently, which is the one failure mode this page cannot survive. So
  `rates::builtin()` carries `as_of` beside the rates, the same way the skill
  ships inside the binary (§gh#133), and `doctor` prints that date and goes
  **not-ok** past 180 days rather than implying freshness. If the automation is
  ever wanted it is a monthly task that opens a **pull request** against the
  table: diffable, human-merged, and a bad parse shows up as a PR nobody
  approves instead of as wrong money on the page. That is a follow-up; this
  ticket only had to make the table a file with a date on it.
- **Unknown model → unpriced, never zero.** `rate_for` answers `None`, the
  caller carries those tokens through as `unpriced`, and every surface prints
  the headline *with* what it left out: `$12.06 at list price … 66k token(s) on
  1 model(s) with no rate, and so not in that total: gpt-5.6-terra`. §gh#96's
  lesson applied to money. The shipped table is Anthropic-only on purpose — this
  file will not carry a price nobody here checked against a published list, and
  a Codex box fixes that with three lines of config rather than by trusting a
  number the board invented.
- **`spend: Option<BoardSpend>` is the "not configured" state, in the type.**
  `None` is *no rates*; a `Some` whose `list_price` is zero is a board that was
  given rates and spent nothing; a `Some` that priced nothing is a window whose
  every model was unknown. Three different sentences, from
  `BoardStats::spend_label`, and no path renders a confident `$0.00` for any of
  them — the same rule `completion_rate` and `token_coverage` already follow.
- **Where the two facts live.** Rates are the board's knowledge, so
  `[defaults.rates."<model>"]` in `routing.toml` — already forwardable (§gh#75)
  and validated with everything else, and a negative rate is refused by name
  because a priced page has no plausibility check of its own. A plan's cost is
  one person's, so `[account."<slot>"]` beside the slot it describes, with an
  `email` to bridge the slot id to the address an attempt actually records.
- **Per account, priced at the models that account ran.** `gather` keeps a
  `(payer, model)` split so an account's figure uses its own models' rates and
  not a board-wide average; the account rows sum to the headline. Alongside it
  `tokens_by_account` joins the `by_account` dispatch counts §gh#101 added —
  those said who ran how many attempts, this says what those attempts spent.
  `plan_in_window` pro-rates the monthly figure over the window (30-day month),
  and is `None` for all time: *how far did the subscription carry it* needs a
  period, and all time is not one.
- **The phone was kept in step.** §gh#157's fixture gained the money rules
  (`humanUsd`, `hasSpend`, `spendLabel`) and two priced `BoardStats` cases, and
  `StatsModels.swift` gained the decode and the same derivations. The Swift half
  was run against the regenerated fixture (118 checks, no drift) — the app's own
  `SpecRunner` still wants its simulator pass, but the rules themselves are
  checked rather than hoped for.

Exit criteria, read back: `BoardStats` carries a priced total derived from
`tokens_by_model` plus the rate set it used and an honest unconfigured/unpriced
state; per-account subscription cost is stored beside the account;
`comet-board stats --json` shows all of it. Comet never sees anybody's actual
bill, and the types do not pretend otherwise — `AccountPlan` is what a human
typed, and nothing on the box can discover it.
