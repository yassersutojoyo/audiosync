# Third-Party Notices

AudioSync's own code is licensed under GPL-3.0-or-later (see
[`LICENSE`](LICENSE)). The app binary contains AudioSync's audio core (`core/`,
written in Rust and under GPL-3.0-or-later like the rest of AudioSync) with
AudioSync's own AirPlay sender (`airplay/`, GPL-3.0-or-later), and, compiled in with them, parts of the Rust standard
library and the third-party crates the sender uses, see
[App frameworks and libraries](#app-frameworks-and-libraries). Every build also
ships the Poppins font (`Contents/Resources/Fonts/`), see [Fonts](#fonts).
AudioSync bundles no separate AirPlay engine.

Each component stays under its own license. The app carries, in
`Contents/Resources/ThirdParty/`:

- this file
- the Poppins font license (`licenses/poppins/OFL.txt`)
- the copyright notices and license texts of the Rust standard library and of
  the Rust crates compiled into the audio core (`licenses/rust/`, the crates in
  `crates.txt`)
- Sparkle's license (`licenses/sparkle/`), when the build includes it

**Earlier versions.** AudioSync releases up to 0.4.1 bundled an AirPlay
engine, OwnTone (GPL-2.0-or-later), and the libraries it used, and attached
their sources to each release. The written offer that came with those builds
still holds: if you received one, you can get the complete corresponding
source of its GPL- and LGPL-licensed components from the maintainer by opening
an issue at <https://github.com/yassersutojoyo/audiosync/issues>, for at least
three years after you received it, at no charge beyond the cost of physically
providing the source.

## App frameworks and libraries

| Component | License |
|---|---|
| [Sparkle](https://sparkle-project.org) 2.9.5 (update framework, currently disabled; license in `licenses/sparkle/`) | MIT |
| [Rust standard library](https://github.com/rust-lang/rust), © The Rust Project Developers (the parts the audio core uses, compiled into the app binary; its copyright notices, which also cover the crates it is built from, its license texts and the Rust version in `licenses/rust/`) | MIT OR Apache-2.0 |

The audio core carries AudioSync's own AirPlay sender (`airplay/`, GPL-3.0-or-later), the
one the Windows app uses, which brings these crates into the app binary:
`mdns-sd` to find receivers on the network, with `flume`, `spin` (MIT),
`lock_api`, `scopeguard`, `mio` (MIT), `socket2`, `socket-pktinfo` (MIT),
`if-addrs` (MIT OR BSD-3-Clause), `fastrand`, `log` and `libc`; `md-5` with
`digest`, `block-buffer`, `crypto-common`, `hybrid-array` and `typenum` for
password digests; `getrandom` with `cfg-if` for random identifiers; for Apple
TV pairing, RustCrypto's `sha1`, `sha2`, `aes`, `aes-gcm`, `ctr` and
`crypto-bigint` with `aead`, `cipher`, `ghash`, `polyval`, `universal-hash`,
`inout`, `cpufeatures`, `cpubits`, `cmov`, `ctutils` and `rand_core`;
`ed25519-dalek`, `x25519-dalek` and `curve25519-dalek` (BSD-3-Clause) with
`ed25519`, `signature` and `subtle` (BSD-3-Clause); `zeroize`; and `plist`
(MIT) for the pairing's property lists, with `quick-xml` (MIT), `time`,
`time-core`, `deranged`, `powerfmt`, `num-conv`, `num-traits`, `base64`,
`indexmap`, `equivalent`, `hashbrown` and `memchr` (Unlicense OR MIT). The
others are under MIT OR Apache-2.0. CI checks every crate in the core's build
with `cargo deny check licenses` against `core/deny.toml`, and
`scripts/build_core.sh` writes each compiled crate's version, license and full
license and notice texts to `licenses/rust/crates.txt`, from `core/Cargo.lock`
(`scripts/core_notices.py`). This sender is how the app plays to AirPlay
speakers.

## Fonts

| Component | License |
|---|---|
| [Poppins](https://github.com/itfoundry/Poppins) Medium, © 2020 The Poppins Project Authors (the wordmark typeface, in `Contents/Resources/Fonts/`; license in `licenses/poppins/OFL.txt`; also in this repository as `brand/fonts/`) | SIL Open Font License 1.1 |

The wordmark SVGs in `brand/` are outlined from Poppins Medium.

## Windows app (in development, not released)

`windows/` is the start of a Windows 11 version of AudioSync. None of it is in
the macOS app, and no Windows build has been released.

| Component | License |
|---|---|
| [Slint](https://slint.dev) 1.18, © SixtyFPS GmbH (the user interface toolkit) | GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0; public builds of AudioSync use it under GPL-3.0-only |
| [tray-icon](https://github.com/tauri-apps/tray-icon) 0.26 and [muda](https://github.com/tauri-apps/muda) 0.21, © the Tauri Programme within The Commons Conservancy (the notification-area icon and its menu) | MIT OR Apache-2.0 |
| [Poppins](https://github.com/itfoundry/Poppins) Medium, © 2020 The Poppins Project Authors (the wordmark typeface, compiled into the executable from `brand/fonts/`) | SIL Open Font License 1.1 |

The app's About dialog, which the top-level menu opens (Help > About
AudioSync), names AudioSync's copyright holder and license and also credits
Slint with its `AboutSlint` widget. The widget shows the Made with Slint logo
(CC-BY-ND-4.0, © SixtyFPS GmbH) unchanged, and the compiled interface also
carries an icon from Microsoft's Fluent UI System Icons (MIT, © Microsoft Corporation), via Slint's Fluent style.

The icons inside the Windows app's controls come from Segoe Fluent Icons, the
icon font Windows 11 ships; the app uses the installed font and doesn't bundle
it. The app and tray icons in `windows/res/` are generated from AudioSync's own
brand mark (`scripts/brand/windows-icons.sh`).

The Windows app also contains AudioSync's own AirPlay sender (`airplay/`,
GPL-3.0-or-later), which brings its dependencies into the executable: `mdns-sd` (Apache-2.0
OR MIT) to find receivers on the network, with the crates it builds on
(`flume`, `spin`, `lock_api`, `scopeguard`, `mio`, `socket2`,
`socket-pktinfo`, `if-addrs` under MIT OR BSD-3-Clause, `fastrand`, `log`,
`windows-sys`, `windows-link`), `md-5` (MIT OR Apache-2.0) with `digest`,
`block-buffer`, `crypto-common`, `hybrid-array` and `typenum` for password
digests, and `getrandom` with `cfg-if` for random session identifiers.

For Apple TV pairing the sender also brings in RustCrypto's `sha1`, `sha2`,
`aes`, `aes-gcm`, `ctr` and `crypto-bigint` with the crates they build on
(`aead`, `cipher`, `ghash`, `polyval`, `universal-hash`, `inout`,
`cpufeatures`, `cpubits`, `cmov`, `ctutils`, `rand_core`), all MIT OR
Apache-2.0; `ed25519-dalek`, `x25519-dalek` and `curve25519-dalek`
(BSD-3-Clause) with `ed25519` and `signature` (Apache-2.0 OR MIT) and
`subtle` (BSD-3-Clause); `zeroize` (Apache-2.0 OR MIT); and `plist` (MIT) for
the pairing's property lists, with `quick-xml` (MIT) and `time`,
`time-core`, `deranged`, `powerfmt` and `num-conv` (MIT OR Apache-2.0), next
to `base64`, `indexmap` and `memchr`, which the app already contained.
`airplay/NOTES.md` lists each with its license; the versions a Windows build
compiles are the ones in `windows/Cargo.lock`, which that build's
`THIRD-PARTY-NOTICES.txt` names.

The Windows app reads and writes its settings file with `serde` and
`serde_json` (MIT OR Apache-2.0), which bring in `serde_core`, `serde_derive`
and `itoa` (MIT OR Apache-2.0) and `zmij` (MIT), next to `memchr`, which the
app already contained.

The Windows app's other Rust dependencies are under permissive licenses. CI
checks them with `cargo deny check licenses` against the list in
`windows/deny.toml`. Each Windows test build CI uploads comes with a
`THIRD-PARTY-NOTICES.txt` next to `audiosync.exe`. `scripts/windows_notices.py`
generates it from `windows/Cargo.lock` for that build's target: every crate
compiled into the executable with its version, license and repository and the
full text of its license and notice files, plus the Rust standard library's
copyright notices, Slint's royalty-free license, and the font and icon
licenses above. It also adds the notices for code that crates took from other
projects, which their own license files don't carry: AccessKit's code derived
from Chromium (BSD-3-Clause, © The Chromium Authors) and the image crate's JPEG
code based on the work of the Independent JPEG Group. The script fails when a
crate's license isn't allowed by `windows/deny.toml`, or when a crate's source
points to a license file the package doesn't ship and the script doesn't
supply.

## Trademarks

AirPlay is a trademark of Apple Inc.; the AirPlay icon in the app is Apple's
SF Symbol, used as the system provides it. The Bluetooth® word mark and logos
are registered trademarks owned by Bluetooth SIG, Inc.; the app draws the
Bluetooth rune in code (`BluetoothRune` in `Sources/AudioSync/MenuBar.swift`)
only to mark speakers connected to this Mac over Bluetooth. AudioSync is not
affiliated with or endorsed by either company.
