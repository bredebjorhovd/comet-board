# Adding a teammate to the box

Five steps, in this order. Each one is cheap; each one is invisible until it is
missing, and three of them fail *after* the teammate's first task has already
run. That is what this page exists for — every mechanism below shipped
separately, and what an operator has to actually do only existed as three
unrelated lines of `comet-board doctor` output.

The short version, for a box that is already running:

```bash
# 1  Settings → Members → invite them by email (an admin has to do this)
# 2  they install comet, `comet login`, and paste the invitation code
comet-board member add ana@example.com --github ana      # 3
# 4  Settings → Agent accounts on the box → sign their Claude/Codex login in
# 5  install the board's GitHub App on whatever repos they bring
comet-board doctor                                       # confirms 3, 4 and 5
```

Everything here is per **person**. Putting a new *repo* on the board is
`comet-board onboard <owner/repo>`, which is a different verb and a different
page.

---

## 1. Invite them to the workspace

Desktop app → **Settings → Members** → the invite form. Only an admin sees it;
the edge decides that (`canInvite`) and a plain member who forces the call gets
a 403 saying so. The invitation shows in the pending list until it is redeemed,
and can be withdrawn from there.

**If you skip it:** nothing else works, and not in a way that says why. Org
membership is what publishes the box's device row to their laptop, what admits
them to the box's device room on the relay, and what lets them open a chat the
board dispatched. Without it `comet-board --device <box> list` from their
machine answers that there is no such device — correctly, from where it is
standing.

## 2. They install comet and paste the code

On their own machine:

```bash
curl -fsSL https://edge.comet.offhand.dev/install.sh | sh
comet login                    # sign in — paste a code, done
```

Then, in the app, the **Join a workspace** field: paste the invitation code
from the email. That scopes their session to your workspace, and the org gate
falls away by itself.

They now have a `comet` and a `comet-board` of their own. They do not need a
box, a checkout, a GitHub credential, or an ssh account — `--device <box>`
forwards every board call to the box that hosts the board, and the work runs
there.

```bash
comet-board --device <box> list          # their first look at the board
export COMET_BOARD_DEVICE=<box>          # or point their whole shell at it once
```

**If you skip it:** there is nothing to skip — this is the step they do.

## 3. Map them: `comet-board member add`

On the box (or anywhere, with `--device`):

```bash
comet-board member add ana@example.com --github ana
```

The first argument is the email **they sign in to comet with** — that is what a
dispatch arrives as, and it is the only thing the board can key on. `--github`
is their GitHub login, resolved through the board's credential into the
`<id>+<login>@users.noreply.github.com` address GitHub minted for that account.
If the lookup is awkward — a login the credential cannot see, no network — they
can read the address off <https://github.com/settings/emails> and you paste it
instead:

```bash
comet-board member add ana@example.com --github 22494697+ana@users.noreply.github.com
comet-board member add ana@example.com --github ana --name "Ana Ruiz"
```

Any address is accepted, not only a noreply one; GitHub attributes a commit to
whichever account holds the address. A noreply address is the one form that is
attributable *by construction*, because GitHub minted it and no other account
can hold it — so `member list` and `doctor` both say which entries took the
weaker form. What is refused is a value that is not an address at all, because
that would land on `GIT_AUTHOR_EMAIL` and produce exactly the unattributable
commits this exists to prevent.

Re-running with the same person changes nothing. Re-running with a different
address corrects the entry rather than adding a second, including when the two
spellings of the email differ only in case.

**If you skip it:** their work commits under the box's own git identity,
whoever released it. Nothing rejects it — the commit lands, the push succeeds,
the PR opens — and it reads as the box owner's on GitHub. On a repo behind a
contributor gate (Vercel attributes a deployment to the commit's *author*, and
on a team plan that attribution is a gate) it is a refused deploy, reported on
the deployment rather than on the push. §gh#107 is the long version.

Under the hood this is `[users]` in the board's `routing.toml`, which was a
hand-edit before gh#162. It still is a file you can open; the verb is what
stops it being the *only* way.

## 4. Give them an agent-account slot

Their runs have to spend a subscription. Without a slot of their own, that is
whichever one the route names — normally the box owner's.

On the **box**, desktop app or TUI → **Settings → Agent accounts** → sign in
their Claude (or Codex) login. It gets a slot id. Then either point the routes
they will work on at it, or have them name it per dispatch:

```bash
comet-board routes list                          # slots and routes, numbered
comet-board routes set 2 account 8f2c1d0a7b6e4539
comet-board dispatch --task gh:acme/app#14 --account 8f2c1d0a7b6e4539
```

A Claude slot and a Codex slot are different subscriptions, and a route's
`runtime` decides which kind it needs; `doctor` refuses a route naming a slot
of the wrong sort rather than failing at dispatch time.

This is the step that pairs with step 3, and the pairing is the thing nobody
thinks about — the two facts live in different places, `routing.toml` and the
engine's saved logins, so nothing put them side by side until `member list`
did:

```
$ comet-board member list
ana@example.com  →  Ana Ruiz <22494697+ana@users.noreply.github.com>
    account 8f2c1d0a7b6e4539 · claude-code · ana@example.com
sam@example.com  →  samito <8134+samito@users.noreply.github.com>
    no agent account — their dispatches spend whichever subscription the route
    names, or the box's own
```

**If you skip it:** their runs spend somebody else's plan. `[defaults]
billing_guard` decides what is said about that — `warn` (the default) says so
in the picker, on the CLI, in the dispatch comment and on the row and releases
anyway; `require-own` refuses unless the release names the payer. It is a
seatbelt and not a lock: the board compares the dispatching frontend's *claim*
about who is signed in against the slot's email, and cannot verify that claim
(gh#161). It catches the accident, not the liar.

## 5. Cover their repos with the App

If they bring work in a repo the board has never seen, the board's GitHub App
has to be installed on it — by whoever owns the repo, on the repos they pick.
What the App can already reach:

```bash
comet-board onboard                    # everything the App can see, and what is on the board
comet-board onboard acme/new-thing     # clone on the box + space + route, one verb
```

**If you skip it:** the repo answers 404 to the board — GitHub does not
distinguish "no such repo" from "not yours", and neither can we — so `onboard`
refuses before anything is cloned and says which of the two credentials needs
widening. A repo already on the board whose installation was later narrowed
fails later and less clearly: polls stop returning issues, and pushes from a
dispatched agent fail on the credential.

---

## What `doctor` confirms

`comet-board doctor` on the box is the check for steps 3, 4 and 5. The three
lines, and which step each one is about:

| line | step | what a healthy one says |
| --- | --- | --- |
| `dispatch authorship` | 3, and 4 | every mapped person, what their address resolves to, and — since gh#162 — which of them has no agent account of their own |
| `git identity` | the box itself | the box has a `git config user.*` at all, so what it commits as *committer* is attributable. A box with none is the one state that FAILs |
| `github auth` / `github app` | 5 | which credential is live, which repos it reaches, and when its token expires |
| `route N: account` | 4 | the slot a route names is one this device has saved, and of the right kind |
| `billing guard` | 4 | what the board does about a dispatch that spends somebody else's subscription |

`dispatch authorship` is always printed and never a failure. No map at all is
the single-operator default and is exactly right on a box only one person
dispatches from — what it must not do is stay invisible, because "everything
lands as the box" and "the map is working" look identical on GitHub until
somebody reads the commit list.

## Taking somebody off

```bash
comet-board member remove ana@example.com
```

Their dispatches commit under the box's identity again. Nothing else about them
changes, deliberately: their agent-account slot is forgotten under **Settings →
Agent accounts**, their org membership is withdrawn under **Settings →
Members**, and the chats they already have stay where they are. Three places
because they are three different revocations, and doing all three from one verb
would make the reversible one look as final as the other two.

## What this is not

None of it is authority. A `[users]` entry is a claim about which GitHub
account a sign-in email belongs to, and the board cannot verify it — its App
may not read anybody's verified addresses. `dispatched_by_user` is unverified
provenance in the same way. What decides what a run may actually *spend* is the
explicit `account` (gh#59); what decides what it may *push* is the board's own
App credential (gh#58). This page is about attribution and about nobody being
surprised, which are worth having on their own.
