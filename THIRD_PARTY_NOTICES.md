# Third-Party Notices

AudioSync's own code is MIT-licensed (see [`LICENSE`](LICENSE)). Builds that
include AirPlay support also ship the AirPlay engine and the libraries it uses
inside `AudioSync.app`:

- `Contents/Helpers/owntone`: the engine (OwnTone)
- `Contents/Frameworks/*.dylib`, `Contents/Frameworks/owntone-sqlext.so`: its libraries

Every build also ships the Poppins font (`Contents/Resources/Fonts/`), see
[Fonts](#fonts). The app binary also contains AudioSync's audio core (`core/`,
written in Rust and MIT-licensed like the rest of AudioSync; its only
third-party crates are test and benchmark tools that never reach the app) and,
compiled in with it, parts of the Rust standard library, see
[App frameworks and libraries](#app-frameworks-and-libraries).

The engine runs as a separate process that AudioSync talks to over a named
pipe and a local HTTP API. Each component stays under its own license. The
app also carries, in `Contents/Resources/ThirdParty/`:

- this file
- OwnTone's license (`OwnTone-COPYING.txt`)
- the exact library versions it was built with (`versions.txt`)
- each library's own license files (`licenses/<library>/`)
- the Poppins font license (`licenses/poppins/OFL.txt`)
- the Rust standard library's copyright notices and license texts (`licenses/rust/`)

The engine as distributed is covered by OwnTone's license, the GNU GPL
version 2 or later. The libraries it links keep their own licenses, all of them
LGPL or permissive and compatible with that: FFmpeg is an LGPL build with no GPL
components, and libunistring, offered under LGPL-3.0-or-later or
GPL-2.0-or-later, is used here under its GPL-2.0-or-later option. AudioSync
itself remains MIT: it is a separate program and does not link against the
engine.

## AirPlay engine

| Component | License | Source |
|---|---|---|
| [OwnTone](https://github.com/owntone/owntone-server) 29.2 | GPL-2.0-or-later | [owntone-29.2.tar.xz](https://github.com/owntone/owntone-server/releases/tag/29.2) |

AudioSync builds OwnTone from that release with eight small source patches.
They are the `perl` edits in [`setup.sh`](setup.sh) (step 3), each documented
inline:

1. `src/outputs/airplay.c`: when a speaker asks for pairing and no saved pairing
   key exists, try transient (PIN-less) pairing instead of a PIN a HomePod
   cannot show.
2. `src/outputs/airplay.c`: report an AirPlay 2 receiver that requires a
   password (and has none in the config file) as needing a PIN or password, so
   the password can be sent through the API's PIN pairing; once paired, use the
   saved pairing key.
3. `src/ptpd.c`: always bind PTP to the wildcard address.
4. `src/misc.c`: bind the AirPlay UDP control/timing sockets to the wildcard
   address while TCP (the API) stays on loopback.
5. `src/httpd_streaming.c`: require the API password for the HTTP audio stream.
6. `src/httpd_rsp.c`: answer every RSP request with 404 (unused protocol; stock
   29.2 frees the request after a refused one and crashes).
7. `src/mdns_dnssd.c`, `src/outputs/raop.c`, `src/outputs/airplay.c`,
   `src/player.c`: remove a speaker that stops advertising. macOS reports the
   removal without an address family, so stock 29.2 keeps the speaker listed
   until the engine restarts; treat it as both families, once the speaker is
   gone from every interface. A speaker renamed while the engine runs keeps its
   entry, so ignore a removal that carries a name the speaker no longer uses.
8. `src/outputs/airplay.c`: when a speaker rejects its saved pairing key,
   don't leave it looking unreachable. A password receiver is marked as needing
   its password again; any other speaker retries transient pairing, and one
   that only pairs with a PIN falls into the stock PIN entry.

OwnTone is built with `MACOSX_DEPLOYMENT_TARGET=15.0` against the libraries
below. Its `configure` sees C library functions the macOS SDK marks as newer
than 15.0 (`pipe2`, `dup3` and a few others, listed in
`scripts/engine_deps.sh`) as absent, so the engine still runs on macOS 15. It
also gets `--without-pulseaudio --without-alsa --without-libwebsockets
--disable-spotify --disable-lastfm --disable-mpd --disable-webinterface`, the
libgcrypt and libgpg-error prefix, and empty protobuf-c flags (the disabled
Spotify client's sub-configure asks for them; nothing is built with them). The
corresponding source is therefore the OwnTone release above plus those edits,
and `./setup.sh` reproduces the exact build.

**Source with each release.** From version 0.4.0 on, every AudioSync release
on [GitHub](https://github.com/yassersutojoyo/audiosync/releases) attaches the
corresponding source of the engine and of every bundled GPL- or LGPL-licensed
library: the OwnTone source tarball (the patches are in `setup.sh` at the
release tag) and, for each library listed below under a GPL or LGPL license,
the exact upstream release tarball it was built from (checked against the
SHA-256 pinned in `scripts/engine_deps.sh`). These libraries are built without
source changes. Next to them are `audiosync-engine-deps.sh`, a copy of
`scripts/engine_deps.sh` with the build steps and configure options, and
`SHA256SUMS-sources.txt`.

**Written offer.** If you received AudioSync as a binary, you can get the
complete corresponding source of every GPL- or LGPL-licensed component it
contains (OwnTone with AudioSync's patches, and the exact versions of the
libraries listed below) from the maintainer: open an issue at
<https://github.com/yassersutojoyo/audiosync/issues>. This offer is valid for at
least three years after you received the binary, at no charge beyond the cost of
physically providing the source.

## Libraries used by the engine

`setup.sh` builds them for macOS 15.0 from checksum-pinned upstream release
tarballs (versions, URLs and SHA-256 sums are in `scripts/engine_deps.sh`), and
they are copied into the app with only their install names, rpaths and code
signatures changed. The exact versions and licenses are in
`Contents/Resources/ThirdParty/versions.txt`.

| Library (bundled files) | License |
|---|---|
| [FFmpeg](https://ffmpeg.org) 9.0.2 (libavcodec, libavformat, libavfilter, libavutil, libswresample, libswscale) | LGPL-2.1-or-later (built without `--enable-gpl` and without external libraries) |
| [libgcrypt](https://gnupg.org/software/libgcrypt/) 1.12.4, [libgpg-error](https://gnupg.org/software/libgpg-error/) 1.61 | LGPL-2.1-or-later |
| [libplist](https://github.com/libimobiledevice/libplist) 2.8.0 | LGPL-2.1-or-later |
| [libunistring](https://www.gnu.org/software/libunistring/) 1.4.2 | LGPL-3.0-or-later OR GPL-2.0-or-later |
| [json-c](https://github.com/json-c/json-c) 0.19 | MIT |
| [libinotify-kqueue](https://github.com/libinotify-kqueue/libinotify-kqueue) 20240724 (libinotify) | MIT |
| [libevent](https://libevent.org) 2.1.13 (libevent, libevent_pthreads) | BSD-3-Clause |
| [libsodium](https://libsodium.org) 1.0.22 | ISC |
| [libConfuse](https://github.com/libconfuse/libconfuse) 3.4 | ISC |
| [SQLite](https://sqlite.org) 3.54.0 (libsqlite3) | Public domain (the dedication at the top of `sqlite3.h`, shipped as its license file) |

The engine also uses macOS's own zlib, libcurl and libxml2, which are part of
the system and not bundled. The sources of the GPL- and LGPL-licensed libraries
above are attached to each release, as described above.

## App frameworks and libraries

| Component | License |
|---|---|
| [Sparkle](https://sparkle-project.org) 2.9.5 (update framework, currently disabled; license in `licenses/sparkle/`) | MIT |
| [Rust standard library](https://github.com/rust-lang/rust), © The Rust Project Developers (the parts the audio core uses, compiled into the app binary; its copyright notices, which also cover the crates it is built from, its license texts and the Rust version in `licenses/rust/`) | MIT OR Apache-2.0 |

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
| [Slint](https://slint.dev) 1.18, © SixtyFPS GmbH (the user interface toolkit) | GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0; used under the Slint Royalty-free Desktop, Mobile, and Web Applications License 2.0 |

That license asks for an attribution. The one AudioSync uses is the
`AboutSlint` widget in the app's About dialog, which the top-level menu opens
(Help > About AudioSync). The widget shows the Made with Slint logo
(CC-BY-ND-4.0, © SixtyFPS GmbH) unchanged, and the compiled interface also
carries a permissively licensed icon from Slint's Fluent style.

The Windows app's other Rust dependencies are under permissive licenses. CI
checks them with `cargo deny check licenses` against the list in
`windows/deny.toml`. Full notices for the Windows app, the Rust standard
library included, will be written before any Windows release.

## Trademarks

AirPlay is a trademark of Apple Inc.; the AirPlay icon in the app is Apple's
SF Symbol, used as the system provides it. The Bluetooth® word mark and logos
are registered trademarks owned by Bluetooth SIG, Inc.; the app draws the
Bluetooth rune in code (`BluetoothRune` in `Sources/AudioSync/MenuBar.swift`)
only to mark speakers connected to this Mac over Bluetooth. AudioSync is not
affiliated with or endorsed by either company.
