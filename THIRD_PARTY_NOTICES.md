# Third-Party Notices

AudioSync's own code is licensed under GPL-3.0-or-later (see
[`LICENSE`](LICENSE)). The app binary contains AudioSync's audio core (`core/`,
written in Rust and under GPL-3.0-or-later like the rest of AudioSync) with
AudioSync's own AirPlay sender (`airplay/`), Google Cast sender (`cast/`) and
web remote (`remote/`), all GPL-3.0-or-later, and, compiled in with them, parts
of the Rust standard library and the third-party crates they use, see
[App frameworks and libraries](#app-frameworks-and-libraries) and
[AudioSync's own Google Cast sender](#audiosyncs-own-google-cast-sender). Every build also
ships the Poppins font (`Contents/Resources/Fonts/`), see [Fonts](#fonts), and
Sparkle, the updater (`Contents/Frameworks/Sparkle.framework`), see
[Sparkle](#sparkle).
AudioSync bundles no separate AirPlay engine.

Each component stays under its own license. The app carries, in
`Contents/Resources/ThirdParty/`:

- this file
- the Poppins font license (`licenses/poppins/OFL.txt`)
- the copyright notices and license texts of the Rust standard library and of
  the Rust crates compiled into the audio core (`licenses/rust/`, the crates in
  `crates.txt`)

**Earlier versions.** AudioSync releases up to 0.4.1 bundled an open-source
AirPlay engine (GPL-2.0-or-later) and the libraries it used, and attached
their sources to each release. The written offer that came with those builds
still holds: if you received one, you can get the complete corresponding
source of its GPL- and LGPL-licensed components from the maintainer by opening
an issue at <https://github.com/yassersutojoyo/audiosync/issues>, for at least
three years after you received it, at no charge beyond the cost of physically
providing the source.

## App frameworks and libraries

| Component | License |
|---|---|
| [Rust standard library](https://github.com/rust-lang/rust), © The Rust Project Developers (the parts the audio core uses, compiled into the app binary; its copyright notices, which also cover the crates it is built from, its license texts and the Rust version in `licenses/rust/`) | MIT OR Apache-2.0 |
| [Sparkle](https://github.com/sparkle-project/Sparkle) 2.10.0, the update framework (`Contents/Frameworks/Sparkle.framework`, from its Swift package; license text below) | MIT, with the BSD-2-Clause, MIT and zlib licenses of the code it includes |

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
for HomeKit pairing, RustCrypto's `chacha20poly1305` and `hkdf` with
`chacha20`, `poly1305` and `hmac`;
`ed25519-dalek`, `x25519-dalek` and `curve25519-dalek` (BSD-3-Clause) with
`ed25519` and `signature` (Apache-2.0 OR MIT) and `subtle` (BSD-3-Clause);
`zeroize`; and `plist` (MIT) for the pairing's property lists, with `quick-xml` (MIT), `time`,
`time-core`, `deranged`, `powerfmt`, `num-conv`, `num-traits`, `base64`,
`indexmap`, `equivalent`, `hashbrown` and `memchr` (Unlicense OR MIT). The
others are under MIT OR Apache-2.0. CI checks every crate in the core's build
with `cargo deny check licenses` against `core/deny.toml`, and
`scripts/build_core.sh` writes each compiled crate's version, license and full
license and notice texts to `licenses/rust/crates.txt`, from `core/Cargo.lock`
(`scripts/core_notices.py`). This sender is how the app plays to AirPlay
speakers.

### Sparkle

Sparkle's `LICENSE` file at 2.10.0, in full:

```text
Copyright (c) 2006-2013 Andy Matuschak.
Copyright (c) 2009-2013 Elgato Systems GmbH.
Copyright (c) 2011-2014 Kornel Lesiński.
Copyright (c) 2015-2017 Mayur Pawashe.
Copyright (c) 2014 C.W. Betts.
Copyright (c) 2014 Petroules Corporation.
Copyright (c) 2014 Big Nerd Ranch.
All rights reserved.

Permission is hereby granted, free of charge, to any person obtaining a copy of
this software and associated documentation files (the "Software"), to deal in
the Software without restriction, including without limitation the rights to
use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software is furnished to do so,
subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR
COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER
IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

=================
EXTERNAL LICENSES
=================

bspatch.c and bsdiff.c, from bsdiff 4.3 <http://www.daemonology.net/bsdiff/>:

Copyright 2003-2005 Colin Percival
All rights reserved

Redistribution and use in source and binary forms, with or without
modification, are permitted providing that the following conditions
are met:
1. Redistributions of source code must retain the above copyright
   notice, this list of conditions and the following disclaimer.
2. Redistributions in binary form must reproduce the above copyright
   notice, this list of conditions and the following disclaimer in the
   documentation and/or other materials provided with the distribution.

THIS SOFTWARE IS PROVIDED BY THE AUTHOR ``AS IS'' AND ANY EXPRESS OR
IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
ARE DISCLAIMED.  IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY
DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS
OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION)
HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT,
STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING
IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
POSSIBILITY OF SUCH DAMAGE.

--

sais.c and sais.h, from sais-lite (2010/08/07) <https://sites.google.com/site/yuta256/sais>:

The sais-lite copyright is as follows:

Copyright (c) 2008-2010 Yuta Mori All Rights Reserved.

Permission is hereby granted, free of charge, to any person
obtaining a copy of this software and associated documentation
files (the "Software"), to deal in the Software without
restriction, including without limitation the rights to use,
copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the
Software is furnished to do so, subject to the following
conditions:

The above copyright notice and this permission notice shall be
included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES
OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT
HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR
OTHER DEALINGS IN THE SOFTWARE.

--

Portable C implementation of Ed25519, from https://github.com/orlp/ed25519

Copyright (c) 2015 Orson Peters <orsonpeters@gmail.com>

This software is provided 'as-is', without any express or implied warranty. In no event will the
authors be held liable for any damages arising from the use of this software.

Permission is granted to anyone to use this software for any purpose, including commercial
applications, and to alter it and redistribute it freely, subject to the following restrictions:

1. The origin of this software must not be misrepresented; you must not claim that you wrote the
   original software. If you use this software in a product, an acknowledgment in the product
   documentation would be appreciated but is not required.

2. Altered source versions must be plainly marked as such, and must not be misrepresented as
   being the original software.

3. This notice may not be removed or altered from any source distribution.

--

SUSignatureVerifier.m:

Copyright (c) 2011 Mark Hamlin.

All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted providing that the following conditions
are met:
1. Redistributions of source code must retain the above copyright
   notice, this list of conditions and the following disclaimer.
2. Redistributions in binary form must reproduce the above copyright
   notice, this list of conditions and the following disclaimer in the
   documentation and/or other materials provided with the distribution.

THIS SOFTWARE IS PROVIDED BY THE AUTHOR ``AS IS'' AND ANY EXPRESS OR
IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
ARE DISCLAIMED.  IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY
DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS
OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION)
HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT,
STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING
IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
POSSIBILITY OF SUCH DAMAGE.
```

## Fonts

| Component | License |
|---|---|
| [Poppins](https://github.com/itfoundry/Poppins) Medium, © 2020 The Poppins Project Authors (the wordmark typeface, in `Contents/Resources/Fonts/`; license in `licenses/poppins/OFL.txt`; also in this repository as `brand/fonts/`) | SIL Open Font License 1.1 |

The wordmark SVGs in `brand/` are outlined from Poppins Medium.

## Windows app (preview)

`desktop/` is the Windows 11 version of AudioSync. None of it is in the macOS
app. Windows builds go out only as an unsigned preview attached to a macOS
release.

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
it. On Linux and macOS those icons are AudioSync's own work, drawn as
hand-written paths in `desktop/ui/widgets.slint`. The app and tray icons in `desktop/res/` are generated from AudioSync's own
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
`cpufeatures`, `cpubits`, `cmov`, `ctutils`, `rand_core`), and for HomeKit
pairing RustCrypto's `chacha20poly1305` and `hkdf` with `chacha20`,
`poly1305` and `hmac`, all MIT OR Apache-2.0; `ed25519-dalek`,
`x25519-dalek` and `curve25519-dalek` (BSD-3-Clause) with `ed25519` and `signature` (Apache-2.0 OR MIT) and
`subtle` (BSD-3-Clause); `zeroize` (Apache-2.0 OR MIT); and `plist` (MIT) for
the pairing's property lists, with `quick-xml` (MIT) and `time`,
`time-core`, `deranged`, `powerfmt` and `num-conv` (MIT OR Apache-2.0), next
to `base64`, `indexmap` and `memchr`, which the app already contained.
`airplay/NOTES.md` lists each with its license; the versions a Windows build
compiles are the ones in `desktop/Cargo.lock`, which that build's
`THIRD-PARTY-NOTICES.txt` names.

The Windows app also contains AudioSync's own Google Cast sender (`cast/`,
GPL-3.0-or-later), described in its own section below. Of the crates listed
there, these reach the Windows executable on top of the ones above:
`native-tls` (MIT OR Apache-2.0) with `schannel` (MIT) for the TLS connection,
`prost` (Apache-2.0) for the protocol buffer messages, and `flacenc`
(Apache-2.0) for the FLAC stream, with `crc`, `crc-catalog`, `heapless`,
`hash32`, `md-5` 0.10, `digest` 0.10, `block-buffer` 0.10 and
`crypto-common` 0.1 (MIT OR Apache-2.0), `generic-array` (MIT) and
`byteorder` (Unlicense OR MIT). The macOS and Linux TLS crates and the
build-time ones aren't compiled into it.

The Windows app also contains AudioSync's own web remote (`remote/`,
GPL-3.0-or-later), whose `getrandom`, `serde_json`, `sha2` and `socket2` the app
already contained, and draws the remote's address as a QR code with `qrcode` (MIT OR
Apache-2.0), built without its optional features, so it brings in no other
crate.

The Windows app reads and writes its settings file with `serde` and
`serde_json` (MIT OR Apache-2.0), which bring in `serde_core`, `serde_derive`
and `itoa` (MIT OR Apache-2.0) and `zmij` (MIT), next to `memchr`, which the
app already contained.

The Windows app's updater, which runs in release builds only, reads the
appcast with `roxmltree` (MIT OR Apache-2.0), checks each download's signature
with `ed25519-dalek` (BSD-3-Clause) and `base64` (MIT OR Apache-2.0), and
unpacks it with `zip` (MIT) and `typed-path` (MIT OR Apache-2.0).

The Windows app's other Rust dependencies are under permissive licenses. CI
checks them with `cargo deny check licenses` against the list in
`desktop/deny.toml`. Each Windows test build CI uploads comes with a
`THIRD-PARTY-NOTICES.txt` next to `audiosync.exe`. `scripts/windows_notices.py`
generates it from `desktop/Cargo.lock` for that build's target: every crate
compiled into the executable with its version, license and repository and the
full text of its license and notice files, plus the Rust standard library's
copyright notices, Slint's license (GPL-3.0-only), and the font and icon
licenses above. It also adds the notices for code that crates took from other
projects, which their own license files don't carry: AccessKit's code derived
from Chromium (BSD-3-Clause, © The Chromium Authors) and the image crate's JPEG
code based on the work of the Independent JPEG Group. The script fails when a
crate's license isn't allowed by `desktop/deny.toml`, or when a crate's source
points to a license file the package doesn't ship and the script doesn't
supply.

## Linux app and AppImage (in development)

The Linux build of `desktop/` shows its tray icon as a StatusNotifier item
through tray-icon's ksni backend: [ksni](https://github.com/iovxw/ksni)
(Unlicense) and [zbus](https://github.com/dbus2/zbus) 5 (MIT), with `png`
(MIT OR Apache-2.0) for the icon. Its text uses the system's font through
fontconfig, and the app includes no font for it. fontconfig, PipeWire,
OpenSSL and the X11, Wayland and OpenGL libraries come from the system at run
time; none of them is part of the program or the AppImage. CI writes the
build's `THIRD-PARTY-NOTICES.txt` with `scripts/windows_notices.py
x86_64-unknown-linux-gnu`, and the AppImage carries it in
`usr/share/doc/audiosync/`.

It captures the whole system's sound through PipeWire with `pipewire`,
`pipewire-sys`, `libspa`, `libspa-sys`, `cookie-factory` and `nom` (MIT),
`bitflags` and `libc` (MIT OR Apache-2.0) and `rustix` (Apache-2.0 WITH
LLVM-exception OR Apache-2.0 OR MIT). Their bindings are generated at build
time with `bindgen` (BSD-3-Clause), `clang-sys` (Apache-2.0), `libloading`
(ISC), `version-compare` (MIT), `system-deps`, `cfg-expr`,
`annotate-snippets` and `cc` (MIT OR Apache-2.0) and `target-lexicon`
(Apache-2.0 WITH LLVM-exception), none of which goes into the program.

It reads the playing track from the MPRIS media players over D-Bus with
`zbus` and its own `zbus_macros`, `zbus_names` and `zvariant` crates (MIT),
and decodes their artwork with `image` (MIT OR Apache-2.0). Every crate
these bring in is under a license `desktop/deny.toml` allows, which CI
checks.

The AppImage file also contains the AppImage runtime, a separate program
that mounts the app and starts it:
[type2-runtime](https://github.com/AppImage/type2-runtime) release 20251108
(MIT, Copyright (c) 2004-23 probonopd). It is linked statically with
[musl](https://musl.libc.org/) (MIT), [libfuse](https://github.com/libfuse/libfuse)
3.15.0 (its library under LGPL-2.1), [squashfuse](https://github.com/vasi/squashfuse)
0.5.2 (BSD-2-Clause), [zstd](https://github.com/facebook/zstd)
(BSD-3-Clause OR GPL-2.0, used under BSD-3-Clause),
[zlib](https://zlib.net) (Zlib) and [mimalloc](https://github.com/microsoft/mimalloc)
(MIT). musl, zstd, zlib and mimalloc come from Alpine Linux 3.21 packages.
The AppImage carries their copyright lines and license texts in
`usr/share/doc/audiosync/RUNTIME-NOTICES.txt` (from
`desktop/linux/RUNTIME-NOTICES.txt`). Each release that ships an AppImage
also carries the source of that runtime release, with its patch to libfuse's
`mount.c`, and of libfuse 3.15.0. appimagetool, which builds the file, isn't
part of it.

## AudioSync's own Google Cast sender

`cast/` is AudioSync's own Google Cast sender (GPL-3.0-or-later). Both apps
contain it: the Windows app links it directly, and the
macOS app through its audio core's C interface, which brings the crates below
into the app binary. It builds on `native-tls` (MIT OR Apache-2.0) for the TLS
connection to the device, with `fastrand` (Apache-2.0 OR MIT) on every
platform and, on macOS, `tempfile`, `errno`, `bitflags` and
`once_cell` (MIT OR Apache-2.0) and `rustix` (Apache-2.0 WITH LLVM-exception
OR Apache-2.0 OR MIT); native-tls uses the platform's TLS stack: `schannel`
(MIT) on Windows, `security-framework`, `security-framework-sys`,
`core-foundation` and `core-foundation-sys` on macOS, and on Linux the system
OpenSSL through `openssl` (Apache-2.0), `openssl-sys` (MIT), `openssl-macros`,
`openssl-probe`, `foreign-types` and `foreign-types-shared`, with `vcpkg` (MIT
OR Apache-2.0) at build time; `prost` (Apache-2.0) for the protocol buffer
messages, with `bytes` (MIT) and, at build time, `prost-derive` (Apache-2.0),
`anyhow`, `itertools` and `either`; `flacenc` (Apache-2.0) to encode the
FLAC stream some devices need instead of WAV, with `crc`, `crc-catalog`,
`heapless`, `hash32`, `stable_deref_trait`, `md-5`, `digest`, `block-buffer`,
`crypto-common`, `generic-array` (MIT), `byteorder` (Unlicense OR MIT),
`num-traits` and, at build time, `rustversion`, `seq-macro`, `version_check`,
`built` (MIT) and `autocfg` (Apache-2.0 OR MIT, through `num-traits`);
`serde_json` (MIT OR Apache-2.0) for the
messages' JSON, with `serde_core`, `itoa` (MIT OR Apache-2.0), `zmij` (MIT)
and `memchr`, in the macOS app as well as the Windows one; and on `mdns-sd`,
`getrandom` and `sha2` with the crates they build on, listed above. The others are under MIT OR Apache-2.0. Its
tests also use `claxon` (Apache-2.0) to decode that FLAC stream, and `rcgen`,
`rustls` (Apache-2.0 OR ISC OR
MIT), `ring` (Apache-2.0 AND ISC), `rustls-webpki` and `untrusted` (ISC) for a
fake device's TLS server, with `rustls-pki-types`, `yasna`, `x509-parser`,
`asn1-rs`, `der-parser`, `oid-registry`, `nom` (MIT), `data-encoding` (MIT),
`time`, `time-core`, `deranged`, `powerfmt`, `num-conv`, `subtle`
(BSD-3-Clause), `zeroize`, `asn1-rs-derive`, `asn1-rs-impl`, `num-bigint`,
`num-integer` and `rusticata-macros` (MIT OR Apache-2.0), and `rsa` and
`rand_core` to make that server an RSA key, with `num-bigint-dig`,
`num-iter`, `pkcs1`, `pkcs8`, `spki`, `der`, `const-oid`, `base64ct`,
`signature`, `smallvec`, `rand`, `rand_chacha`, `ppv-lite86` (MIT OR
Apache-2.0), `zerocopy` and `zerocopy-derive` (BSD-2-Clause OR Apache-2.0 OR
MIT) and `libm` (MIT); those are never part of a build that ships. CI
checks every crate of `cast/`, development dependencies included, with `cargo
deny check licenses` against `cast/deny.toml`.

## Trademarks

AirPlay is a trademark of Apple Inc.; the AirPlay icon in the app is Apple's
SF Symbol, used as the system provides it. The Bluetooth® word mark and logos
are registered trademarks owned by Bluetooth SIG, Inc.; the app draws the
Bluetooth rune in code (`BluetoothRune` in `Sources/AudioSync/MenuBar.swift`)
only to mark speakers connected to this Mac over Bluetooth. AudioSync is not
affiliated with or endorsed by either company.
