# The spend band, and where the list price goes — **done** (gh#225)

Part of gh#171, and the second pass over the block §gh#179 built. That block
was right about the order of the page and wrong about the room: it gave a whole
card to one sentence, three figures wrapped across it at 32px gaps, and then
spent nothing on the fact a reader needs to *act* on the headline.

**The headline was not actionable, and the missing half was already computed.**
`$248 at list price` tells you what a week cost. It does not tell you that
$93 of it was a million output tokens and $52 of it was thirty-five million
cached ones — which is the difference between "the cache is working, the
writing is what costs" and "something is replaying context it should not".
§gh#182 prices the four buckets apart precisely because they are nowhere near
each other, and then every surface added them straight back up. The split was
one regrouping away the whole time.

### One band, not three cards

The three figures are one sentence read left to right — *this cost that, the
plan carried it this far, and it went mostly there* — so they are cells of one
row separated by a hairline, not three boxes with three borders between them.
Each is a figure with its evidence under it rather than beside it:

| | |
|---|---|
| `$248` | `at list price · 42.0M tokens over the last 7 days` |
| `10.9×` | `$23 of a $100/mo plan over the same last 7 days` |
| `37%` | `of the price is output, on 1.24M tokens` |

The middle cell is the change of emphasis: the multiple was the third figure
and is now the second, with the two amounts demoted to its caption. *How far
does the subscription carry this* is the question; the dollars are how the
answer was arrived at.

The rule §gh#182 set survives the compression — the list price and the plan are
never added, only divided, and `monthly_phrase` counts plans rather than
summing several teammates' bills into one implied figure (§gh#59). So do the
three ways there is no second figure: no plan entered reads `—` and *not
entered*, an all-time window says it has no days to pro-rate onto, and a plan
that costs nothing has no ratio because a ratio against zero is not a number.

### Where the list price goes

The new block is a proportional bar over the four cost classes and a legend
carrying **both** numbers per class:

```
output $93 / 1.24M   cache writes $73 / 3.90M   cached input $52 / 34.8M   uncached input $32 / 2.10M
```

Both, always. Dollars alone hide that the cheap class is the enormous one;
tokens alone hide that the small one is the bill. Read together they are the
only view on this page that answers *which of the four rates is expensive
here*, which is a different question from the per-model table's *which model
is* — a week can be 90% one model and still be a week where the cache is the
story.

- **`BoardSpend::cost_split` is arithmetic, not collection.** Nothing new is
  recorded and nothing is re-priced: these are the same four products
  `ModelRate::cost` already sums per model, kept apart across models instead of
  collapsed. Which is why `CostSplit::total` is `list_price` *exactly* rather
  than nearly — same terms, rounded in the same places — and a test pins that,
  because a breakdown that does not add up to the figure above it is the one
  thing a money page must never ship.
- **Priced models only**, exactly like `list_price`. A model the table has
  never heard of has no rate to attribute its tokens to, and inventing one to
  fill a segment would put a number under a bar that means nothing. What it
  spent stays where it was: the unpriced line under the band.
- **Ranked on money, tied on rate.** `CostClass` is declared dearest-per-token
  first and the split sorts on what was actually spent, so the bar reads
  dark-to-light left to right and an unchanged board redraws identically. A
  class nobody spent in is absent, not a zero-width segment nobody can see —
  `ranked_tokens`' rule, one axis over.
- **One hue at four weights, not four hues.** The status ramp (gh#173) means
  something on this page; borrowing a status colour to say `cache writes` would
  be a colour lying about what it names.

### The empty states are still prose

The exit condition worth restating: a bar of four zeroes is not an empty state.
No rates, rates that matched nothing, and nothing metered each keep the
sentence `spend_label` already owns, and the band and the bar are simply absent
— there is no path that draws a chart of nothing. There is one new case:
`CostSplit::is_empty` is also true when a window's whole price rounds to zero
(a table of free models), and that keeps the band and says so in the third cell
rather than drawing four empty segments.

The footnote shrank to the one line it always should have been: *Comet never
sees your bill: list prices come from a dated table, plan costs from Accounts.*
Same claim, one line, at the bottom of the money it qualifies.

### Not in this ticket

The split has no Swift counterpart yet — the phone's stats screen draws no bar,
so there is nothing there to disagree with the rule and no case in the
cross-language fixture (`hour_grid` is in the same position, for the same
reason). Nothing on the wire changed: `CostSplit` is derived from the
`BoardSpend` a board already sends, so an older box answers a newer viewport
and the block draws anyway. gh#181 is where the phone catches up.
