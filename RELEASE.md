# How to release AudioSync

## How releases are signed

I don't have an Apple Developer account, so releases go out on GitHub signed with the self-signed "AudioSync Local Code Signing" identity that `make-local-cert.sh` creates on the first `./build.sh`. They aren't notarized and don't use the Hardened Runtime (`build.sh` only enables it for a `Developer ID Application` identity). The README tells users how to get past Gatekeeper.

`build.sh` signs ad-hoc only with `AUDIOSYNC_ADHOC=1`, or when there's no local identity and `make-local-cert.sh` can't create one, and then it says so loudly. If the local identity exists but can't sign, the build stops before it quits or replaces the installed app. Avoid ad-hoc for anything you ship: an ad-hoc signature changes on every build, so the System Audio Recording permission goes stale (the toggle stays on but capture is silent).

Build every release on the same Mac with the same identity, so updates keep the same designated requirement and users' privacy permissions carry over. That makes the local key a release key. Keep a private backup of its keychain (`~/Library/Keychains/audiosync-signing.keychain-db`) together with its password file (`signing-keychain.pass` in the AudioSync Application Support folder). If either one is deleted, for example by following the uninstall steps on the release Mac, the next build creates a new identity and every user has to grant the permissions again.

## Cutting a release

First bump `CFBundleShortVersionString` and `CFBundleVersion` in `Info.plist`, set the same version in `desktop/Cargo.toml` and in the `audiosync-desktop` entry of `desktop/Cargo.lock`, and in `CHANGELOG.md` put the release date on the version's heading (it waits there as `## [x.y.z] - unreleased`; if the entries sit under `## [Unreleased]` instead, move them under a new heading). Point that version's link at the bottom to `releases/tag/vx.y.z`, like the older versions, since the tag exists once you push it below, and commit. Once the release is out, open the next version's heading as `## [x.y.z] - unreleased` with a `compare/vx.y.z...HEAD` link, so later changes have a place to go. Then:

```bash
./build.sh --release                        # the release flag turns the updater on
Tests/run-tests.sh --hardware
Tests/soak.sh                               # 60 min, silent; must end with VERDICT: PASS

VERSION=x.y.z
NOTES=path/to/release-notes.md          # your local notes file, not committed
./make-dmg.sh                               # → AudioSync.dmg
ditto -c -k --keepParent /Applications/AudioSync.app "AudioSync-$VERSION.zip"
scripts/sign_appcast.sh "$VERSION"          # → appcast.xml, for the app's updater
cp scripts/install.sh install.sh
shasum -a 256 AudioSync.dmg "AudioSync-$VERSION.zip" appcast.xml install.sh THIRD_PARTY_NOTICES.md > SHA256SUMS.txt

git tag -a "v$VERSION" -m "AudioSync $VERSION" && git push origin "v$VERSION"
gh release create "v$VERSION" --title "AudioSync $VERSION" --notes-file "$NOTES" \
  AudioSync.dmg "AudioSync-$VERSION.zip" appcast.xml install.sh SHA256SUMS.txt THIRD_PARTY_NOTICES.md
```

`scripts/install.sh` (the Terminal install in the README) depends on two of these: it downloads `AudioSync-$VERSION.zip`, expects `AudioSync.app` at the top of it, and checks it against that file's line in `SHA256SUMS.txt`. Keep both names and the `--keepParent` zip as they are. The copy attached to the release lets someone install that exact version with the script that shipped with it.

The app updates itself with Sparkle from `releases/latest/download/appcast.xml`, so every release needs its own `appcast.xml` next to the zip. Sparkle checks each download against the EdDSA public key in `Info.plist` (`SUPublicEDKey`). The key pair comes from Sparkle's `generate_keys` (in `.build/artifacts` after `swift build`, under `Sparkle/bin`), run once on the release Mac: it keeps the private key in the login keychain and prints the public key. Paste that public key into `Info.plist` once and commit it; it isn't a secret. The key alone doesn't turn updates on: only `./build.sh --release` adds `AudioSyncReleaseBuild` to the built app's `Info.plist`, and the app checks for updates only with both, so a plain `./build.sh` and CI builds never check. `--release` stops when the key is missing, and `scripts/sign_appcast.sh` refuses a zip whose app lacks the key or the flag. Back the private key up the same way as the signing keychain (`generate_keys -x` exports it): without it, released builds can't take another update.

Run `Tests/soak.sh` for the full 60 minutes before every release and only go on when it ends with `VERDICT: PASS`. It's silent and unattended, and judges against `Tests/baseline/rust.json` ([docs/BENCHMARKS.md](docs/BENCHMARKS.md)). If that baseline is missing, the soak says `NO BASELINE` and exits 3: run `Tests/bench.sh` first (an hour), commit the baseline and the updated `docs/BENCHMARKS.md`, then run the soak.

Write your release notes file before tagging. Start from the CHANGELOG entry, keep them short and user-facing, and include the install notes (macOS 15 or later, Open Anyway). Leave the title out of the file, since `--title` sets it.

Publish it as a normal release, not a pre-release. The README and the site link to `/releases/latest`, which skips pre-releases, so the download link would keep pointing at the old version.

## Windows preview (optional)

The Windows build can ride along with a Mac release as an unsigned preview, but only after the maintainer has tried that same commit on a real Windows 11 PC: start it with `AUDIOSYNC_DEBUG=1`, play to a local output and a Bluetooth speaker, listen, and check `%TEMP%\audiosync-debug.log` for errors. Without that try, no Windows files go on the release. Take `audiosync-windows-x64` and `audiosync-windows-arm64` from the Windows CI run of the release commit (CI keeps them for 7 days; re-run the workflow if they're gone), and attach each as a zip of its `audiosync.exe` with the `THIRD-PARTY-NOTICES.txt` from the same artifact, for example `AudioSync-$VERSION-windows-x64.zip`. Add both zips to the `shasum` and `gh release create` commands above, so they're in `SHA256SUMS.txt` and on the release.

Say plainly in the release notes that it's an unsigned preview (SmartScreen will warn on first launch), Windows 11 only, and what its AirPlay reaches: receivers that take AirPlay 1, with a password prompt for a receiver that has one and a PIN prompt for an Apple TV that asks for a code, while AirPlay 2 stays experimental and off unless File > AirPlay 2 (Experimental) is on (and whether each was tried on a real speaker or Apple TV). The app carries Slint's attribution in its About dialog (see `THIRD_PARTY_NOTICES.md`), so the release notes need no badge.

## Linux AppImage

A release with a Linux build attaches, from the Linux CI run of the release commit, the `AudioSync-$VERSION-x86_64.AppImage` and its `.sha256` from `audiosync-linux-x86_64`, and both files from `appimage-runtime-sources`: `type2-runtime-20251108.tar.gz` (the runtime's source with its libfuse patch) and `fuse-3.15.0.tar.xz` (libfuse, LGPL-2.1). The runtime inside the AppImage links libfuse statically, so its source goes with every copy. Add all of them to the `shasum` and `gh release create` commands above.

## Sources and license obligations

The Mac app bundles no third-party GPL or LGPL code: it is AudioSync's own code (GPL-3.0-or-later), the Rust standard library and the crates the audio core compiles in, and the Poppins font, the third-party parts all under permissive licenses. Every release ships `THIRD_PARTY_NOTICES.md` (it's also inside the app, with the license files). A new dependency gets its license checked and its entry in the notices before it ships.

The Windows preview is different: it compiles in Slint, used under GPL-3.0-only, so that exe as a whole goes out under GPL-3.0 only. A release that carries the Windows zips also attaches their source: run `cargo vendor` in `desktop/` at the release commit and attach the repository tree at that commit plus the `vendor/` folder as one archive, with the `[source]` snippet that `cargo vendor` prints saved as `desktop/.cargo/config.toml` so the archive builds offline, for example `AudioSync-$VERSION-windows-source.tar.gz`, listed in `SHA256SUMS.txt` like the other files.

Releases up to 0.4.1 bundled an open-source AirPlay engine (GPL-2.0-or-later) and its libraries, and attached their sources. The written offer for those builds in `THIRD_PARTY_NOTICES.md` still has to be honored, so their source archives stay in a private backup for at least three years.

## Notarized releases (not used yet)

With a Developer ID, releases would get the Hardened Runtime and notarization, and users wouldn't have to approve the first launch. People on the locally signed releases would have to grant the privacy permissions once more, because the designated requirement changes with the certificate.

You'd need an Apple Developer Program membership, a Developer ID Application certificate in your login keychain, and a notarytool profile (`xcrun notarytool store-credentials audiosync-release --apple-id … --team-id … --password <app-specific-password>`). Any macOS can build the release: `build.sh` builds the core and the app for `LSMinimumSystemVersion` (macOS 15.0), and `Tests/check-app-bundle.sh` checks that the binary targets it.

```bash
# Give the identity by its full name; a SHA-1 hash would skip the Hardened Runtime
# and entitlements. AudioSync.entitlements (microphone, Apple Events) is applied
# automatically.
CODESIGN_IDENTITY="Developer ID Application: <name> (<team ID>)" ./build.sh
APP=/Applications/AudioSync.app   # or wherever build.sh installed it

ditto -c -k --keepParent "$APP" AudioSync.zip
xcrun notarytool submit --wait --keychain-profile audiosync-release AudioSync.zip
xcrun stapler staple "$APP"
spctl -a -vvv --type execute "$APP"   # should say: accepted, source=Notarized Developer ID
```

Then zip the stapled app again for the release, or build the DMG with the same `CODESIGN_IDENTITY ./make-dmg.sh` and notarize and staple that too. Other entitlements can be passed with `ENTITLEMENTS=path/to.entitlements ./build.sh`.

## Other ways to distribute

Building from source with `./setup.sh` works today and needs no developer account, since an app built on your own Mac isn't quarantined. A Homebrew cask would be possible later, but it needs a separate `yassersutojoyo/homebrew-tap` repository, and none exists yet.

AudioSync uses four privacy permissions (Screen & System Audio Recording and Microphone for capture, Local Network for AirPlay, Automation for Now Playing), driven by the usage strings in `Info.plist`. Under the Hardened Runtime, the microphone and Apple Events also need the entitlements in `AudioSync.entitlements`.
