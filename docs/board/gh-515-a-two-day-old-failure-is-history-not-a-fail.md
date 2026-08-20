# A two-day-old failure is history, not a FAIL — **done** (gh#515)

`doctor` printed this, in red:

```
FAIL dispatched pushes    the askpass helper answers, and mints per push; `gh` at
                          /opt/homebrew/bin/gh is wrapped to mint per call ·
                          last recorded failure: 2026-08-17T15:02:59.362623+00:00
                          unusable bredebjorhovd/comet-board via dispatch: … Error:
                          github HTTP 504 for /repos/bredebjorhovd/comet-board: We
                          couldn't respond to your request in time.
```

Read in full, that line says the credential path is **healthy**: the helper
answers, `gh` is wrapped, both in the present tense. The red came from a failure
two days old whose cause was GitHub returning `504`. The operator read it as
"`gh` has dropped off the PATH" and spent a morning on a wrapper that was never
broken — while the two checks that actually answer that question, `agent PATH`
and `gh stack`, both said `ok` a few lines up.

### What was wrong

§gh#233 added the ledger read for a good reason: a failure inside somebody's
real run is a fact the probe cannot reach, because the probe is not that run. It
then applied it with no rule at all —

```rust
let detail = match credential_ledger::last_failure(paths) {
    Some(last) => { ok = false; … }
```

— so *any* recorded failure, of any age, with any cause, and even one the path
had demonstrably worked past since, turned the line red for good. A ledger is
append-only; a check written this way can never go green again.

### The rule now

[`crate::doctor::push_verdict`] is the whole of it, and it is a pure function of
(probe, standing failure, now) so the four readings are four tests:

- **The probe fails** → red, and the probe's sentence *leads*. It is the one
  naming something to fix; the ledger trails it as dated context.
- **The probe passes and the standing failure was GitHub's own** — a 5xx, a
  gateway timeout, a rate limit → `ok`, at any age. Nothing on this box caused
  it and nothing on this box will fix it. The only action it could ask for is
  "wait", and red does not mean wait.
- **The probe passes and a *local* failure is younger than a day** → still red,
  and now the actionable sentence leads. This is gh#233's shape exactly: the
  probe cannot mint against the API and cannot be the run that failed, so a
  fresh failure it cannot reproduce is still the thing to act on. The line says
  where to look — the run's own log, not this box's config.
- **Anything older, or minted past** → history, on an `ok` line, in the past
  tense: `· history: the last failure was GitHub's own, 2d ago — …`.

Three pieces carry it, all in `crates/board/src/credential_ledger.rs`:

- **`standing_failure`** replaces `last_failure`. It walks back from the tail and
  stops at an [`Event::Minted`], returning `None`: a mint is the helper answering
  a real prompt on a real run, which is a stronger statement about the path than
  anything that went wrong before it. This is the "aged out by a subsequent
  success" half, and it is what lets the check go green without anybody editing
  a file.
- **`Entry::cause`** reads `Upstream` or `Local` off the error text the caller
  quoted — a 5xx in the `HTTP <status>` shape the GitHub client produces, or one
  of eight phrases. Read rather than recorded, because the writers are inside
  git's credential prompt holding a string somebody else handed them, and asking
  them to classify would be asking the layer with the least context. Anything
  unrecognised is `Local`, which is the reading that keeps the check loud: an
  unknown cause becomes a quiet false alarm, never a silent real one.
- **`FRESH_SECS`**, one day. Long enough that this morning's failed dispatch
  still shouts; short enough that the one from the day before yesterday has
  stopped.

`doctor` is what you run when the board looks wrong. A red check that is neither
current nor actionable spends the operator's attention on infrastructure that is
fine, which is the failure mode `doctor` exists to prevent.
