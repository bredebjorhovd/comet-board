# Installing Comet on macOS

## First launch

1. Open the `.dmg` and drag **Comet.app** onto **Applications**.
2. Run this once:

   ```sh
   xattr -dr com.apple.quarantine /Applications/Comet.app
   ```

3. Open Comet normally.

Step 2 is not optional on current releases, and skipping it produces a dialog
that explains nothing. The same instructions ship inside the dmg as
`READ ME FIRST.txt` (source: [`dist/macos/DMG-README.txt`](../dist/macos/DMG-README.txt)).

## Why

Releases are built by CI without an Apple Developer ID, so `Comet.app` carries
only an **ad-hoc** signature — a checksum with no developer identity behind it,
and no notarization ticket from Apple:

```console
$ spctl --assess --type execute /Applications/Comet.app; codesign -dv /Applications/Comet.app
/Applications/Comet.app: rejected
Signature=adhoc
```

macOS tags every downloaded file with the `com.apple.quarantine` extended
attribute. On first launch of a quarantined bundle Gatekeeper looks for a
signature it can trace to a registered developer, finds an anonymous one, and
refuses. The user-facing failure is the unhelpful *"Comet can't be opened"* /
*"Appen kan ikke åpnes"* dialog, sometimes with error `-50`.

Two things worth knowing, because both cost time to rediscover:

- **Right-click → Open does not reliably help.** That escape hatch is offered
  for *unsigned* and Developer-ID-but-unnotarized apps. For an ad-hoc signature
  macOS often declines to offer it at all. Recent macOS may surface an *Open
  Anyway* button under **System Settings → Privacy & Security** immediately
  after a failed launch; when it appears it works, but do not count on it.
- **`comet update` is unaffected.** The in-app updater
  ([`crates/update`](../crates/update)) downloads the `-app.tar.gz` artifact
  over HTTP and extracts it itself. Nothing in that path sets the quarantine
  attribute, so updates keep working after the first manual unblock.

Deleting the attribute is a deliberate statement that you trust this copy of
the app. Do it for builds from
[the project's releases](https://github.com/bredebjorhovd/comet-board/releases),
not for a `Comet.app` that arrived some other way.

## One extra tool, if the board you joined stacks

Nothing above is affected by this, and the app does not need it. It is here
because this is the page you are on while setting your own machine up, and
because nothing else would tell you.

A board can dispatch a task as a **stack** — one task decomposed into layered
pull requests, each a dependent concern, reviewed in parallel instead of as one
wall of diff (§gh#287). GitHub builds those with a `gh` extension, and reading
one is the same extension:

```sh
brew install gh                          # if you have not already
gh auth login                            # your own account, your own machine
gh extension install github/gh-stack

gh stack view                            # the chain, from any branch in it
gh stack checkout 12                     # move around it by pull request number
```

Without it those pull requests still open, still notify you and still review
normally — GitHub's web UI draws the stack either way. What you lose is the
CLI's view of them: `gh pr list` shows five requests with nothing saying which
one sits under which, and `gh stack` answers `unknown command "stack"`.

This is per machine, and yours is separate from the box's. The box needs its own
copy for the agents to *write* stacks with, which is an operator step on the box
and not something your laptop can supply — see
[`docs/teammate.md`](teammate.md).

## Making the step go away (operator)

The workaround exists only because nobody has bought an **Apple Developer
Program** membership (~$99/yr) for this project. Once one exists, the release
pipeline already knows what to do — it just needs the secrets.

`scripts/package-macos.sh` signs with the hardened runtime, notarizes, and
staples whenever `CODESIGN_IDENTITY` plus notary credentials are present, and
falls back to today's ad-hoc path when they are not.
[`.github/workflows/release.yml`](../.github/workflows/release.yml) wires that
up from repository secrets. Set these under **Settings → Secrets and variables
→ Actions**:

| Secret | What it is |
| --- | --- |
| `MACOS_CERTIFICATE_P12` | base64 of the exported *Developer ID Application* certificate + private key (`.p12`) |
| `MACOS_CERTIFICATE_PWD` | the password used when exporting that `.p12` |
| `MACOS_SIGNING_IDENTITY` | the identity's full name, e.g. `Developer ID Application: Your Name (TEAMID)` |

plus notary credentials, in **one** of two forms — the App Store Connect API
key is the better one (revocable, no account password anywhere):

| Secret | What it is |
| --- | --- |
| `MACOS_NOTARY_KEY` | base64 of the App Store Connect API private key (`AuthKey_*.p8`) |
| `MACOS_NOTARY_KEY_ID` | the key's ID |
| `MACOS_NOTARY_ISSUER_ID` | the issuer UUID from App Store Connect |

or:

| Secret | What it is |
| --- | --- |
| `MACOS_NOTARY_APPLE_ID` | the Apple ID email on the developer account |
| `MACOS_NOTARY_PASSWORD` | an **app-specific** password, not the account password |
| `MACOS_NOTARY_TEAM_ID` | the 10-character team ID |

Exporting the certificate, once:

```sh
# In Keychain Access, having installed the Developer ID Application cert:
#   right-click the certificate → Export → .p12, set a password
base64 -i Comet-DeveloperID.p12 | pbcopy      # → MACOS_CERTIFICATE_P12
security find-identity -v -p codesigning      # → MACOS_SIGNING_IDENTITY
```

With `MACOS_CERTIFICATE_P12` absent, the macOS release job behaves exactly as
it does today. With it present it imports the certificate into a throwaway
keychain, signs, notarizes, staples — and the dmg stops shipping the README,
because the step it describes is no longer needed.

Verifying a signed build, on a machine that never built it:

```sh
spctl --assess --type execute -vv /Applications/Comet.app   # → accepted, source=Notarized Developer ID
xcrun stapler validate /Applications/Comet.app              # → The validate action worked!
```
