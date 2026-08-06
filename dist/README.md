# Packaging

## Linux (implemented)

```sh
scripts/package-linux.sh            # release build (thin LTO, stripped)
PROFILE=debug scripts/package-linux.sh   # fast smoke package
```

Produces `target/package/comet-<version>-linux-<arch>.tar.gz` containing:

- `comet` — the binary (headed by default; `comet headless` runs the engine alone)
- `comet.desktop` — XDG desktop entry
- `comet.png` — 1024×1024 app icon (the comet mark from the original app;
  vector source `comet.svg`)
- `install.sh` — installs into `~/.local/{bin,share/applications,share/icons}`

The release profile in the root `Cargo.toml` sets `lto = "thin"` and
`strip = "symbols"` for distribution builds.

## macOS

```sh
scripts/package-macos.sh    # → target/package/comet-<version>-macos-<arch>.dmg
```

Builds the release binary, assembles `Comet.app` (Info.plist + icns), signs it,
and wraps it in a dmg (with an `/Applications` symlink). CI runs this on tags
(`.github/workflows/release.yml`).

The script ends by printing `spctl`'s verdict on what it just built, which is
the only thing that matters to a downloader:

- **No `CODESIGN_IDENTITY`** (today's releases) → ad-hoc signature, Gatekeeper
  rejects a downloaded copy. The dmg then also carries `READ ME FIRST.txt`
  (from `dist/macos/DMG-README.txt`) with the `xattr -dr com.apple.quarantine`
  one-liner that unblocks it.
- **`CODESIGN_IDENTITY` + `MACOS_NOTARY_*`** → hardened-runtime signed,
  notarized and stapled (both the app and the dmg), Gatekeeper accepts, and the
  README is left out because there is nothing to work around.

See [docs/macos-install.md](../docs/macos-install.md) for the user-facing
instructions and the secrets that switch CI onto the signed path.

The manual steps the script automates, for reference (run on a macOS host —
gpui needs Metal; no cross-build from Linux):

1. Build the universal (or per-arch) binary:
   ```sh
   cargo build --release -p comet --target aarch64-apple-darwin
   cargo build --release -p comet --target x86_64-apple-darwin
   lipo -create -output comet \
     target/aarch64-apple-darwin/release/comet \
     target/x86_64-apple-darwin/release/comet
   ```
2. Assemble the bundle:
   ```sh
   mkdir -p Comet.app/Contents/{MacOS,Resources}
   cp comet Comet.app/Contents/MacOS/comet
   sed "s/__VERSION__/$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')/" \
     dist/macos/Info.plist > Comet.app/Contents/Info.plist
   ```
3. Icon: generate `comet.icns` from `dist/comet.png` (`iconutil`) and place it at
   `Comet.app/Contents/Resources/comet.icns`:
   ```sh
   mkdir comet.iconset && sips -z 256 256 dist/comet.png --out comet.iconset/icon_256x256.png
   iconutil -c icns comet.iconset -o Comet.app/Contents/Resources/comet.icns
   ```
4. Sign + notarize (required for distribution):
   ```sh
   codesign --deep --force --options runtime --sign "Developer ID Application: …" Comet.app
   xcrun notarytool submit Comet.zip --keychain-profile … --wait
   xcrun stapler staple Comet.app
   ```
5. Ship as a `.dmg` (`hdiutil create -volname Comet -srcfolder Comet.app -ov -format UDZO Comet.dmg`).
