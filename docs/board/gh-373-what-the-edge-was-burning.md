# The edge was never rate-limited. It wrote 102,517 rows into a 100,000-row day (gh#373)

Brede, 2026-08-12: *"I am consistently hitting Cloudflare Durable Object limits
— I have 25,000 failed calls now. I would be happy to upgrade to the $5 plan,
but I do not want to end up with a huge CF bill because of this."*

The right worry, and the right order to answer it in. **Upgrade first and a
runaway loop stops being an error and starts being an invoice.** So: what is
generating the calls, and only then, what does the plan cost.

Everything below is read from Cloudflare's own analytics for account
`0cac004a6d08e46e625eafa75b3ab625` on 2026-08-12, via the GraphQL API and
`wrangler tail`. Nothing was deployed, changed, or upgraded.

## The limit, in Cloudflare's words

Every single failure — 684 of 684 captured live on `wrangler tail`, one distinct
message — is this:

```
Error: Exceeded allowed rows written in Durable Objects free tier.
    at ensureUpdateLog (index.js:10248:7)
    at new SessionRoom (index.js:10364:5)
```

and its `DeviceRoom` twin (`at new DeviceRoom (index.js:11385:21)`).

That is the **Workers Free plan's SQLite rows-written cap: 100,000 rows written
per day.** Not a request limit. Not a subrequest limit. Not the daily
100,000-request cap, which we were nowhere near.

`durableObjectsPeriodicGroups` gives the count that tripped it:

| hour (UTC) | rowsWritten | cumulative | rowsRead | DO requests | of which threw |
|---|---|---|---|---|---|
| 09:00 | 80 | 15,196 | 3,801 | 272 | 0 |
| 10:00 | 1,137 | 16,333 | 18,024 | 450 | 0 |
| 11:00 | 15,862 | 32,195 | 71,236 | 2,965 | 0 |
| **12:00** | **63,283** | **95,478** | 263,212 | 11,195 | 0 |
| **13:00** | **7,039** | **102,517** | 63,010 | 4,287 | **2,980** |
| 14:00 | 0 | 102,517 | 0 | 3,700 | 3,699 |
| 15:00 | 0 | 102,517 | 0 | 3,050 | 3,050 |
| 16:00 | 0 | 102,517 | 0 | 5,077 | 5,077 |
| 17:00 | 0 | 102,517 | 0 | 5,975 | 5,975 |
| 18:00 | 0 | 102,517 | 0 | 3,494 | 3,494 |

The 12:00 hour wrote 62% of the entire day's allowance. 13:00 crossed 100,000.
From 14:00 onward **every** Durable Object call fails and the row counter is
frozen, because the throw happens in the constructor — before any handler runs.

## What the 25,000 actually are

They are all downstream of that one cap. Per-day, per-class, from
`durableObjectsInvocationsAdaptiveGroups`:

| date | DO requests | `scriptThrewException` | rowsWritten | rowsRead | activeTime (s) |
|---|---|---|---|---|---|
| 08-06¹ | 2,633 | 5 | 2,752 | 38,750 | 16,945 |
| 08-07 | 16,105 | 0 | 6,350 | 129,983 | 119,204 |
| 08-08 | 19,169 | 0 | 31,554 | 495,114 | 107,325 |
| 08-09 | 17,472 | 10 | 55,487 | 843,881 | 51,352 |
| 08-10 | 9,918 | 5 | 13,264 | 197,965 | 44,170 |
| 08-11 | 16,382 | 48 | 39,572 | 579,443 | 81,223 |
| **08-12** | **46,486** | **23,761** | **102,517** | 675,047 | 71,998 |

¹ partial — 08-06 is the edge of the analytics retention window.

**23,761 of the 23,829 failures are today, and 23,756 of those are after 13:00
UTC.** Before today the six-day total was 68. This is not a chronic condition
that finally got noticed; it is one day, one cap, one cliff.

Note the last column of the failure hours: **3,000–6,000 failed calls per hour,
sustained, against a pre-cap baseline of 300–500 total calls per hour.** After
the cap the edge is ten times busier than it ever was working. That is the
retry traffic, and it is what turned a broken afternoon into a five-digit
number.

### The retries are bounded, and that matters

`crates/sync/src/room.rs:139-140` — `BACKOFF_BASE` 250 ms doubling to a
`BACKOFF_CAP` of 30 s, reset only on a successful join. So each room still being
dialed retries at most twice a minute. A 13-minute `wrangler tail` capture saw
23 distinct Durable Object ids retrying; 23 rooms at the 30 s cap is ~2,800
calls/hour, the same order as the 3,000–6,000/hour observed. **There is no
runaway loop.** The failure count is large because it is a couple of dozen
clients × 30-second retries × six hours, not because anything is spinning.

This is the finding that decides the plan question: the traffic is *bounded and
self-limiting*, so upgrading does not uncork anything. On the paid plan the cap
that causes these failures does not exist at this volume, the constructor stops
throwing, joins succeed, and the retries stop by themselves.

### It is not the alarm chain

The first hypothesis was `SessionRoom.alarm()` retrying forever on Cloudflare's
backoff — the one mechanism here that makes DO calls with no client connected.
It is not that:

- The throw is in the **constructor** (`ensureUpdateLog`, `new DeviceRoom`), not
  in `alarm()`. Nothing gets far enough in to reach the backup logic.
- Every captured failure carries a client `request` — `/ws`, `/session/*/ws`,
  `/device/*/ws`, `/status`. There are no alarm invocations in the failure set.
- The last deploy was **2026-08-09T18:03Z**, three days before the cliff. The
  code did not change.

The alarm chain does have a real consequence, though: since 13:00 UTC the daily
alarm cannot construct its DO either, so **the nightly R2 backup
(`backup/{chatId}/latest.loro`) has not run today.** `backupDirty` stays `1`, so
it will catch up on its own once writes are possible again.

## The defect: we write three rows per update that we did not need to write

The cap was hit by real traffic, but the multiplier is ours.
`SessionRoom.recordLoroUpdates` (`edge/src/session-room.ts:940-944`) runs on
**every inbound update batch** and does:

```ts
this.setMeta("tailDirty", "1");
this.setMeta("backupDirty", "1");
this.setMeta("postReset", "0");
```

`setMeta` (`:323`) is an unconditional
`INSERT … ON CONFLICT DO UPDATE SET value = excluded.value`. SQLite performs the
update whether or not the value changed, and Cloudflare bills every updated row
as a row written.

The update-log rows themselves are batched — `scheduleFlush` debounces by
`DO_FLUSH_MS = 5_000` (`edge/src/session-doc/constants.ts:65`), so a burst of
updates costs one `appendUpdateRow` per update plus one `updateBytes` meta write
per *flush*. The three meta writes above are **not** batched. So for any room
receiving more than one update per five seconds — which is every active agent
session — these dominate the row count.

And after the first batch of a burst, all three are writing values that are
already there. `tailDirty` is only cleared by a `/tail` read; `backupDirty` only
by the daily alarm; `postReset` only by a wedge-break reset. In a stream of *K*
update batches, 3*K* rows are written and roughly 3(*K*−1) of them change
nothing.

That is a genuine defect and it is worth fixing regardless of the plan, because
it is the difference between a 100,000-row day and a comfortable one. **The fix
is in a separate pull request** — this document is the finding Brede needs in
order to decide about the plan, and the two should not be entangled.

The fix itself is one line in `setMeta`: read before writing, and return early
when the value is unchanged. A row read costs 1/50th of a row-write's daily
allowance on free and 1/1000th of its price on paid, so trading a write for a
read is strictly good on both plans.

## The cost ceiling, in numbers

Workers Paid is **$5/month**, and it includes a Workers Paid allowance for each
Durable Objects dimension. Our five clean days (08-07 → 08-11) average:

- **15,809** DO requests/day
- **29,245** rows written/day
- **449,277** rows read/day
- **80,655** active-seconds/day

Duration is billed as wall-clock active seconds × 128 MB (Cloudflare allocates
128 MB per DO for billing regardless of actual use), i.e. **× 0.125 GB-s per
second**.

| dimension | included (Paid) | our rate, ×30 days | % of included | overage |
|---|---|---|---|---|
| Requests | 1,000,000/mo, then $0.15/M | 474,270 | 47% | **$0.00** |
| Duration | 400,000 GB-s/mo, then $12.50/M | 302,456 GB-s | 76% | **$0.00** |
| Rows written | 50,000,000/mo, then $1.00/M | 877,350 | 1.8% | **$0.00** |
| Rows read | 25,000,000,000/mo, then $0.001/M | 13,478,310 | 0.05% | **$0.00** |
| Stored data | 5 GB-month, then $0.20/GB-mo | not billed yet² | — | **$0.00** |

² `durableObjectsStorageGroups` returns empty for this account, and no storage
limit has ever thrown. The free plan's storage cap is the same 5 GB the paid
plan includes, so this dimension cannot get worse by upgrading.

**Expected bill on Workers Paid at current volume: $5.00/month, with nothing
added on top.**

### And if it is not fixed

The question that actually matters. Three adversarial cases:

1. **The retry storm never stops.** Take the worst observed failure hour
   (5,975 calls) and run it 24/7 for a month: 4,302,000 requests → 3,302,000
   over the included million → **$0.50/month**. Failed calls throw in the
   constructor, so they burn no duration and write no rows. (This scenario is
   also self-refuting — on Paid the cap is not hit, so the throws stop.)
2. **Today's worst *write* hour, forever.** 63,283 rows written × 24 × 30 =
   45.6M rows/month — still **inside** the 50M included. **$0.00.** Even the
   hour that broke the free plan, sustained round the clock for a month, does
   not produce a charge on Paid.
3. **What would $10/month of overage actually take?** Rows written: 60M/month =
   2M/day, which is **68× our average** and 20× the day that broke. Duration:
   1.2M GB-s/month = 320,000 active-seconds/day, **4× our average** and 2.7× our
   worst day (08-07, 33 DO-hours).

So the honest ranking of risk is: **duration is the only dimension within one
order of magnitude of a real charge**, and it is the one to watch — not requests
and not rows. It sits at 76% of the included allowance today, and 08-07 alone
ran at 14,900 GB-s, which pro-rated is 112% of a day's share. A month of 08-07s
would cost $0.59 over the $5.

Hibernation is being used correctly (`ctx.acceptWebSocket` in both classes, plus
the `setWebSocketAutoResponse` ping/pong pair), so that active time is genuine
work — Loro imports and replays — not idle sockets held open.

## Recommendation

**Upgrade.** The concern that motivated the question — that the plan change
converts a runaway into a bill — does not survive the data. There is no runaway;
there is a bounded retry against a daily cap, and the paid allowances sit
between 1.3× and 1,850× our actual usage depending on the dimension. The
realistic bill is $5.00, and the pessimistic bill is $5.59.

Independently of the plan, three things should be capped in code:

- **`setMeta` must not write a row to store a value that is already stored.**
  The one high-confidence fix; separate PR.
- **An alarm that fails consecutively should give up.** Not the cause here, but
  the code comment at `session-room.ts:1442` describes an intentionally
  unbounded retry chain, and on a paid plan an alarm that throws every time bills
  every attempt. A consecutive-failure counter in meta, giving up after N, costs
  nothing and removes the only unbounded call source in the system.
- **Nothing needs a reconnect-backoff change.** It is already capped at 30 s and
  behaved exactly as designed through a six-hour total outage.
