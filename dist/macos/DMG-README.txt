Comet — first launch on macOS
=============================

1. Drag Comet.app onto the Applications folder in this window.

2. Run this once in Terminal:

       xattr -dr com.apple.quarantine /Applications/Comet.app

3. Open Comet normally (Launchpad, Spotlight, double-click).


Why step 2?
-----------
This build is not signed with an Apple Developer ID and is not notarized —
it carries only an ad-hoc signature. macOS flags anything you download with
a "quarantine" attribute, and Gatekeeper refuses to launch a quarantined app
it cannot trace back to a registered developer.

What you see instead is an unhelpful dialog — "Comet can't be opened" /
"Appen kan ikke åpnes", sometimes with error -50 — and for an ad-hoc
signature the usual right-click → Open escape hatch is often not offered at
all. (On recent macOS you may find an "Open Anyway" button under System
Settings → Privacy & Security right after a failed launch; when it is there
it works, but it is not reliably offered for this case.)

Clearing the quarantine attribute is what actually lets the app start. You
are telling macOS that you trust this copy, so only do it for a Comet.app
from a release you trust:

    https://github.com/bredebjorhovd/comet-board/releases

Repeat step 2 after each manual download. In-app updates (`comet update`)
are not affected — they never pass through quarantine.

This step disappears once the project ships a Developer ID-signed, notarized
build; then the app launches with nothing extra.
