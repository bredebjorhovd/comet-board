# A refusal arrives with GitHub's words — **done** (gh#338)

Leaving a comment through the review window on v0.4.0 answers 422, and nobody on
the box can say why. `Rest::send` reported every failed call as
`bail!("github HTTP {} for {path}")` and dropped the response body on the floor —
and GitHub's 422 always carries a `message`, usually with an `errors` array
naming the field that was unprocessable. The one sentence explaining the failure
was read off the wire and thrown away.

Landed in `crates/board/src/sources/github.rs` (`github_reason`, `refusal`,
`HttpRest::interpret`, `credential_failure`, `Github::post_review`) and
`HttpAppApi::send` in `crates/board/src/sources/github_app.rs`.

**This does not fix the 422.** It makes the 422 say what it is. The hypotheses
on the issue are still hypotheses; the next comment through the review window is
now a diagnosis rather than a dead end.

### One place, not one verb at a time

gh#318 taught `put_reply` to hand back the status alongside the body, because
the asynchronous merge endpoint needed the status as *data* — 202, 200, 409 and
400 are four different answers, not one verdict. Reading the reason out came
along for the ride, and only that verb got it.

The generalisation is not `post_reply` and `patch_reply` beside it. Those
callers do not want the status as data; they want the failure to explain
itself, which is a property of *every* call and belongs where every call already
converges. So the reason is dug out in `HttpRest::interpret` — the one funnel
`get`, `post`, `patch` and `put` all pass through — and `put_reply` is left
exactly as it was, because it is the genuine exception rather than the pattern.

`github_reason` composes what GitHub sent:

```
github HTTP 422 for /repos/o/r/pulls/508/reviews: Validation Failed
  — user_id: Can not approve your own pull request
```

The field is kept alongside the message rather than dropped, because the field
*is* the diagnosis: `Validation Failed` alone does not tell a body GitHub will
not take apart from an identity that may not cast this verdict, and those two
want opposite fixes. Entries arrive as objects, and occasionally as bare
strings; both render. A body with nothing usable in it — a 204, an array, a page
of HTML from something sitting in front of GitHub — falls back to the bare
`github HTTP {status} for {path}` that was there before, so nothing that reads
these strings loses its footing. `doctor` matching `404` still matches: the
status stays in the message and stays first.

### The 403 that was never a rate limit

`credential_failure` named every 403 a rate limit. GitHub spends 403 on two
unrelated things — an actual rate limit, and a permission it will not grant this
credential at all (`Resource not accessible by integration`, which is what a
missing `Pull requests: write` looks like from an installation token). Only the
body tells them apart, so the board was sending whoever read that line to wait
out a clock that was never running. A 403 whose body says something now says
what it says; one that says nothing still reads as a rate limit.

The 401 keeps its credential hint and gains GitHub's words after it, rather than
in place of them — the hint is what an operator acts on.

### The verdict names itself

`Github::post_review` wraps its refusal with `submitting the {event} verdict on
{repo}#{number}`. Which verdict was refused is half of reading a 422 here: a
board App reviewing a pull request its own App opened may `COMMENT` on it but
cannot `APPROVE` it, so `COMMENT` failing and `APPROVE` failing are different
findings. The RPC layer already formats with `{e:#}`, so the context and
GitHub's own words both reach the review window on one line.

### The App's own credential, too

`HttpAppApi::send` had the same bug on the minting path — a JWT or an
installation-token request refused with a bare status. Whoever installed the App
got `github HTTP 422` and no clue which half of the key was wrong. It now reads
the body first and reports through the same `refusal`.

### Not in this issue

Why the review window's 422 happens. The three candidates on the ticket —
reviewing a pull request the board's own App opened, a body GitHub will not
take, a permission shape — are untested, and testing them takes one comment
through the review window on a dispatched PR against the live box, which this
change is the prerequisite for rather than a substitute for. If the answer turns
out to be the self-review case, the fix is about identity and belongs in its own
issue; nothing here presumes it.
