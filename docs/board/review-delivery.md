# Review delivery — **done**

Landed as `crates/board/src/review.rs`: `SyncEngine::deliver_reviews`, run by
the board loop after every sync cycle against the pulls that cycle already
polled. Per-PR per-endpoint watermarks (`meta` under `reviews:<task>`), the
`updated_at` gate, the first-sight floor, and the actionability filter came
over verbatim; delivery is `Runtime::prompt` — a durable ledger entry, steer
or send. Dropped as planned: the wake latch and busy-check — the ledger
queues into a busy chat safely and supersede rules handle pileups. The honest
consequence is documented at the top of `review.rs`: with no author to key on
and no latch, an agent's own PR reply is relayed back into its chat once (the
composed message says so), instead of herdr-board's trade of swallowing human
comments that landed inside the wake window; the watermark still makes it a
single bounce. The author check survives as `Runtime::chat_alive` plus a new
`Runtime::chat_cwd` — the chat row's cwd must still be the attempt's
checkout.
