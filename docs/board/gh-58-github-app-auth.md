# GitHub App auth — **done** (gh#58)

The board takes **either** a personal access token or a GitHub App, and prefers
the App when both are configured. `.env`:

```
GITHUB_TOKEN=ghp_…                        # a personal access token
GITHUB_APP_ID=123456                      # or a GitHub App
GITHUB_APP_PRIVATE_KEY_PATH=/…/app.pem    # (chmod 600)
```

`GITHUB_TOKEN` keeps working exactly as it did, and is still the right answer
for a board watching your own repos. It stops being the right answer the moment
somebody else wants the board on *their* repos: a PAT belongs to an account and
carries that account's whole reach, and a fine-grained one is scoped to a single
resource owner — so two owners would need a classic PAT's blanket `repo` scope.
An App is installed by the repo owner, on the repos they pick, with no credential
changing hands. Rate limits become per installation, offboarding is an uninstall
rather than a rotation, and writes land as `[bot]`, which is honestly not a
human.

**The seam** is `sources/github_app.rs`: `TokenProvider` — `Anonymous` /
`Static(pat)` / `App` — replaces `HttpRest`'s `Option<String>` token. Under an
App the credential depends on where the request is going: a `/repos/{owner}/{repo}/…`
path gets that repo's installation token, and the App's own endpoints get a
freshly signed JWT (RS256, `iat` backdated 60s for clock skew, `exp` 9 minutes
out — inside GitHub's ten-minute ceiling). Two caches, each keyed on what the
fact belongs to: repo → installation, installation → token. Keying the token by
**installation** rather than by repo is what makes six repos behind one
installation cost one mint instead of six, and is what will let one board process
serve several installations later. Tokens refresh five minutes before their
stated expiry.

A **401 under an App re-mints once and retries, then gives up**. An installation
token lives an hour and the world can change inside one, so a stale token is
worth exactly one retry; a genuinely revoked installation answers 401 to the
fresh token too, and a board that kept re-minting would spin against GitHub
forever rather than fail and be fixed. A PAT never takes that path — there is
nothing to invalidate, so its 401 is final, as it always was.

`jsonwebtoken` is the crate's first crypto dependency, pinned to 9.x because
that line signs through `ring`, which rustls already builds into this workspace;
10+ dropped it for `aws-lc-rs` (cmake in the release pipeline) or `rust_crypto`
(the `rsa` crate, and RUSTSEC-2023-0071).

`comet-board doctor` reports which mode is live, the App's slug and every
installation with its account, the private key's permissions, and each repo's
installation and token expiry.

**Pushing** (`git_credentials.rs`) needs a token too, and the token must not be
written into `.git/config` — it expires in an hour and the checkout does not.
`push_url` carries only the username (`x-access-token`); `push_env` points
`GIT_ASKPASS` at `comet-board git-askpass`, which mints at push time and writes
the token to the pipe git is holding. Nothing lands in argv, in `.git/config`,
or in the environment — all three are readable by other processes on a box that,
since #55, several people drive. The box's own credential helper is switched off
for the push, so an hourly token cannot end up cached in the keychain. Who runs
that push is §gh#70.

Operator work, not the agent's: register the App, set **Issues: RW, Pull
requests: RW, Contents: RW, Metadata: R** (Contents write is what `merge_pr`'s
`PUT /pulls/{n}/merge` needs), generate the key, make the App public so others
can install it, and drop the PEM on the box.

Deliberately out of scope, each its own ticket: webhooks replacing polling (a
separate delivery path with its own endpoint and secret), and repo
auto-discovery via `GET /installation/repositories` replacing the manual
`[github] repos` list (it changes what "polled" means).
