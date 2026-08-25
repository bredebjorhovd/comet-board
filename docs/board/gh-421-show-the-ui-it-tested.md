# Show the UI it tested — review evidence artifacts (gh#421)

The review surface's evidence is unusually strong on text: the brief, the
agent's claims (§gh#235), the effects the board derived itself (gh#236), the
run's commands and how they exited (§gh#183), and the diff. Every one of those
answers a question about **code**. None of them answers the question a
frontend change actually asks: *what did it look like when you ran it?*

An agent can say it opened the app, exercised the flow and checked the result.
The reviewer receives none of that — no durable artifact, tied to the attempt
and its commit, viewable without reopening the chat. This ticket makes
visual/runtime evidence a first-class review input rather than one more
sentence in a final answer.

### The model

One attempt may publish bounded **evidence artifacts**
(`comet_board::evidence::EvidenceArtifact`). Five kinds, and the kind decides
the ceiling:

| kind | what it is | cap |
|---|---|---|
| `screenshot` | viewport or full-page PNG/JPEG/WebP | 10 MiB |
| `recording` | short interaction capture (WebM/MP4) | 24 MiB |
| `accessibility` | accessibility-tree snapshot | 1 MiB |
| `console` | console/network excerpt | 256 KiB |
| `log` | test/dev-server log excerpt | 1 MiB |

Eight artifacts per attempt, description ≤ 300 chars, URL ≤ 2048,
viewport spelled `WxH`. Bounds are enforced where the artifact is accepted —
the board's host — so every client gets the same refusal (the claims rule:
a contract enforced in the client is a contract the next client does not have).

Provenance splits by who authored it, which is the whole design:

**The agent supplies** the kind, the bytes, the URL it captured, the viewport,
and one sentence on what the artifact demonstrates.

**The board stamps everything else**, reading it from facts it already holds
rather than from anything the agent typed:

- task and attempt — the row the artifact lands on;
- producing chat — the attempt's `pane_id`;
- capture receipt time — the board clock;
- commit SHA and dirty-file count — read out of the attempt's own worktree with
  `git rev-parse HEAD` and `git status --porcelain` **at attach time**, never
  taken from the agent's word;
- byte size and SHA-256 — computed over the received bytes.

The fingerprint half matters most. A screenshot whose provenance says "commit
`a1b2c3d`, three files uncommitted" pins pixels to code state the way a claim's
anchor pins a sentence to the diff — and like everything else on this screen,
it is evidence the agent did not author. An agent that captured before its last
commit cannot say otherwise: the board looked at the tree after receiving the
file. What that costs is stated below, under "what it does not prove".

Identity is content-addressed within the attempt: id is `<kind>-<sha8>`, and
attaching bytes the attempt already has is a **no-op returning the current
review**, not a duplicate row and not an error — a retried call after a lost
reply must not double-store. The same bytes under a different kind are a
different artifact; a screenshot and the console excerpt of the same page say
different things.

### Lifetime

1. **Capture.** Off-board, by whatever the environment already has — see
   "capture lanes" below. Comet runs nothing to capture in this slice.
2. **Attach.** `comet-board evidence --task <id> --kind <kind> --file <path>
   [--url …] [--viewport WxH] [--description …]`. The CLI reads the file,
   sends it once as base64 (`AttachBoardEvidence`, relay-forwardable like every
   board verb), and the board validates, fingerprints, stores and records.
3.  **Store.** Bytes live under `{data_dir}/board/state/evidence/<attempt>/`
   on the board's host (`Paths::state_dir` + `evidence/`), named by their
   content address. The typed record rides the attempt row
   (`attempts.evidence_artifacts`, JSON, nullable) beside `changed_files` and
   `run_evidence` — loaded when somebody opens a review, not on every cycle.
4. **Review.** `ReadAttemptReview` carries the artifacts; `comet-board review`
   prints them; the desktop review window and the phone render them (UI pass
   specified below). Reading pixels back is `ReadAttemptEvidence`
   (`{taskId, attempt?, id, offset}` → chunked base64), resolved **by id on
   the board's host** — the caller never names a path, so there is no path
   jail to escape: an unknown id has no file, and a known id has exactly one.
5.  **Expire.** Metadata lives as long as the attempt row — tiny, and the
   provenance stays true even after the pixels are gone. Bytes age out at
   `RETAIN_EVIDENCE_DAYS` = 90, swept per file by mtime on the gc interval
   (`SyncEngine::sweep_expired_artifacts`, beside `collect_worktrees`), empty
   dirs pruned. After
   expiry a review shows the record and the fact of expiry instead of the
   image — "expired" is a finding about age, never silently blank.

Why bytes expire while claims do not: a checkout is reclaimed (gh#72) and its
snapshot kept because a sentence is cheap and load-bearing forever; ten
screenshots per attempt at up to 24 MiB each is the `target/` lesson again
(gh#186) — bulk kept past its usefulness becomes the thing that fills the box.

### Capture lanes — the comparison this spike owed

**Lane A — engine-hosted headless Chromium/CDP.** The box drives its own
headless Chrome over CDP: load URL, wait, screenshot / full-page capture /
accessibility tree / screencast-to-WebM. Works on the primary deployment —
a headless always-on box, where the agents and the app-under-test already are.
Deterministic viewport; no human watching required; artifacts produced where
they are stored. Costs: a browser runtime the board has to find or bundle and
keep current; real memory on a box that already runs several agents; CDP
version churn; and login state needs explicit handling (below).

**Lane B — a visible desktop browser panel.** Comet's window hosts the page;
the agent drives it via CDP; the human can watch live. Login comes free (the
user's own profile). But it presumes a headed desktop session on the box —
which the fork's primary deployment explicitly does not have (`comet
headless` is the point of the box) — and it couples evidence capture to
someone watching, which inverts the relationship: evidence exists for reviews
that happen when nobody is looking.

**Decision: Lane A is the product's lane**, because the box is headless more
often than not. It is also **not built yet**. Slice one is deliberately
lane-agnostic: the attach verb accepts a file from anywhere, which makes every
existing tool the capture path — `npx playwright screenshot --viewport-size=…`,
`chrome --headless=new --screenshot=… --window-size=WxH <url>`, macOS
`screencapture`, a Playwright script writing WebM. One agent run can already
capture one screenshot against its local app and attach it; the recipe ships
in the skill. A later slice adds `comet-board evidence capture --url …`
on top of Lane A when the file path proves too loose (agents skipping
captures entirely is the failure to watch for, and the fix then is making
capture one command, not building a policy engine now).

Route-level requirements ("frontend tasks must carry a screenshot") are named
by the issue and deliberately deferred with it: the first slice builds the
artifact and its plumbing, not a policy engine.

### Where the existing attachment/R2 path fits, and where it does not

Reused: the **transport shape** — chunked base64 reads sized for the relay,
host-first reads proxied through the owning device, exactly how attachment
read-back works. An artifact read is org-gated the way every other board verb
is, by being forwardable through the same relay.

Not reused: **storage in R2.** Edge attachments are user-scoped blobs behind a
bearer; attempt evidence is board-scoped and its natural authz is the board's
own relay gate. Mirroring into the shared content-addressed space would buy
offline viewing on other devices at the cost of a second authz story and a
second retention problem. Named trigger for revisiting: a device that must
render artifacts while the box is offline. Until then, offline means the
metadata renders and the pixel fetch fails with a nameable error.

### Remote host, offline, size, authenticated pages

**Remote host.** Everything works relayed: attach from the agent's shell
(wherever it sits), review from any device. Bytes never leave the board's
host except as chunks to authorized readers.

**Offline / local mode** (`COMET_EDGE_URL=off`). Nothing here touches the
edge. Attach, store, review and read-back are all engine-local.

**Size limits.** Refused at attach with the number in the refusal; stored
once (content address); served chunked. A recording that arrives at 40 MiB is
told the cap, not trimmed.

**Authenticated pages.** Comet takes no position on how the app under test
authenticates and holds no credentials for it — the agent logs in however it
already does (dev mode, seeded session, its own driven browser). Two rules
are stated rather than solved: the URL field records where the capture was
taken, never a token (strip queries); and console excerpts are the leak risk
of this feature — guidance says trim secrets before attaching, and the
description is the place to say what was redacted. A future Lane A recorder
would need storage-state handling designed separately; that design starts
from "Comet never stores the app's credentials", not from convenience.

**Retention.** See Lifetime step 5: metadata permanent, bytes 90 days from
receipt, swept on the gc interval, expiry visible in the review rather than
silent.

### What an artifact proves, and what it does not

**It proves:** at commit `C` (with `D` files uncommitted), a page at URL `U`,
rendered at viewport `V`, produced these pixels / this accessibility tree /
this log — received by the board at time `T`. That much is machine-checked:
the fingerprint, size and hash are the board's own readings.

**It does not prove:**

- **That the code was right.** Pixels are not a spec. A screenshot of a
  beautifully-rendered wrong answer is still a wrong answer, rendered well.
- **That the flow was exercised.** A screenshot says a page loaded; only a
  recording (or the run journal's commands around it) suggests interaction,
  and neither says the interactions were the ones that mattered.
- **That the pixels match the attached commit.** The board fingerprints at
  *attach* time; capture happened earlier, on the agent's honour. An agent
  that captures, then keeps hacking, attaches stale pixels against a fresh
  fingerprint. Mitigation is sequencing guidance (attach immediately after
  capture) and the dirty count — a large `D` beside fresh commits is the
  visual tell — but the gap is real and the review should read it.
- **That the server served this branch's build.** A dev server can be hours
  stale (gh#186's cache lesson, wearing a different hat). The artifact proves
  what was rendered, not that what was rendered was built from `HEAD`.
- **Accessibility conformance.** An accessibility snapshot is a tree, not an
  audit. "The tree was this" is not "the axe report was clean".
- **That nothing else was broken.** Evidence is additive testimony, never a
  verdict: nothing here folds into `findings()` or moves the verdict bar, for
  the reason gh#236's chips did not. The unclaimed set stays the loudest
  voice on the screen; four screenshots shouting would leave it nothing to
  shout with.

### Review rendering

Desktop — **landed with this ticket** (`crates/ui/src/review.rs`): a
label-and-body band in the review window's own shape, "Captures" in the 62px
label column, between Evidence and Claims — the seen thing with the measured
things, above the story. Screenshots render as 16:10 thumbnails (the viewport
shape a desktop capture almost always has); caption line is
`viewport · sha8 · N uncommitted` in mono over the truncated description; a
click opens the full image in the same lightbox the transcript's thumbnails
use, so one gesture means one thing everywhere in the app. Fetches start when
the review lands (not when the band scrolls into view), through
`ReadAttemptEvidence` chunks into the shared attachment cache under a
synthetic key — the cache is generic over its key strings, and evidence
differs from a chat attachment only by addressing: id, never path. A failed
or expired fetch draws the dashed card captioned `unavailable`, which reads as
a fact about the artifact rather than as a bug in the app. Non-image kinds
(recording, accessibility, console, log) draw honest cards naming their kind —
a recording nobody can play here must not draw as a broken image.

Phone and richer viewers are the named follow-up: a horizontal card rail on
the iOS review screen, inline playback for recordings, and a scrollable text
view for excerpts. None of them opens the authoring chat to show any of this —
that is exit criterion three, and the whole reason the bytes ride the attempt
row rather than the transcript.

### License boundary

Zuse (pinned `swarajbachu/zuse@a97865e…`) informed the *behavior*: bounded
artifacts with hard caps, structured provenance serialized into review
context, recordings capped in both duration and bytes. Zuse is AGPL-3.0-only;
nothing here copies its source, schemas, UI, or tests. The caps, the kind set,
the field names and the storage layout are this repo's own, derived from this
issue and comet-board's existing patterns (attempt-row snapshots, chunked
attachment reads, content addressing).
