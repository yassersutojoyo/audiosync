# Third-Party Notices

AudioSync's own code is MIT-licensed (see [`LICENSE`](LICENSE)). Builds that
include AirPlay support also ship the AirPlay engine and the libraries it uses
inside `AudioSync.app`:

- `Contents/Helpers/owntone`: the engine (OwnTone)
- `Contents/Frameworks/*.dylib`, `Contents/Frameworks/owntone-sqlext.so`: its libraries

Every build also ships the Poppins font (`Contents/Resources/Fonts/`), see
[Fonts](#fonts).

The engine runs as a separate process that AudioSync talks to over a named
pipe and a local HTTP API. Each component stays under its own license. The
app also carries, in `Contents/Resources/ThirdParty/`:

- this file
- OwnTone's license (`OwnTone-COPYING.txt`)
- the exact library versions it was built with (`versions.txt`)
- each library's own license files (`licenses/<formula>/`)
- the Poppins font license (`licenses/poppins/OFL.txt`)

Because the bundled FFmpeg is built with GPL-3.0 components, **the engine as
distributed is covered by the GNU GPL version 3**. AudioSync itself remains MIT:
it is a separate program and does not link against the engine.

## AirPlay engine

| Component | License | Source |
|---|---|---|
| [OwnTone](https://github.com/owntone/owntone-server) 29.2 | GPL-2.0-or-later | [owntone-29.2.tar.xz](https://github.com/owntone/owntone-server/releases/tag/29.2) |
| [libinotify-kqueue](https://github.com/libinotify-kqueue/libinotify-kqueue) 20240724 (license in `licenses/libinotify-kqueue/`) | MIT | [release](https://github.com/libinotify-kqueue/libinotify-kqueue/releases/tag/20240724) |

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

`configure` also gets `ac_cv_func_pipe2=no ac_cv_func_dup3=no`, so a binary
built with the macOS 27 SDK still runs on macOS 26 and earlier. It also gets
`--without-pulseaudio --without-alsa --without-libwebsockets --disable-spotify
--disable-lastfm --disable-mpd --disable-webinterface`. The corresponding
source is therefore the OwnTone release above plus those edits, and
`./setup.sh` reproduces the exact build.

**Source with each release.** From version 0.4.0 on, every AudioSync release
on [GitHub](https://github.com/yassersutojoyo/audiosync/releases) attaches the
corresponding source of the engine and of every bundled GPL- or LGPL-licensed
library: the OwnTone source tarball (the patches are in `setup.sh` at the
release tag) and, for each library listed below under a GPL or LGPL license,
the exact upstream source (the release tarball, or an archive of the pinned git revision), the Homebrew formula it was built from (build
options and patches), each external patch that formula applies, and
`SHA256SUMS-sources.txt`.

**Written offer.** If you received AudioSync as a binary, you can get the
complete corresponding source of every GPL- or LGPL-licensed component it
contains (OwnTone with AudioSync's patches, and the exact versions of the
libraries listed below) from the maintainer: open an issue at
<https://github.com/yassersutojoyo/audiosync/issues>. This offer is valid for at
least three years after you received the binary, at no charge beyond the cost of
physically providing the source.

## Libraries used by the engine

They are installed from Homebrew when the engine is built, and copied into the
app with only their install names, rpaths and code signatures changed. The
exact formula versions are in `Contents/Resources/ThirdParty/versions.txt`.

| Library (bundled files) | License |
|---|---|
| FFmpeg 9 (libavcodec, libavformat, libavfilter, libavutil, libswresample, libswscale) | GPL-3.0-or-later (Homebrew builds it with `--enable-gpl --enable-version3`) |
| x264 (libx264), x265 (libx265) | GPL-2.0-or-later |
| libgcrypt, libgpg-error, libplist | LGPL-2.1-or-later |
| gettext (libintl) | LGPL-2.1-or-later |
| mpg123 (libmpg123) | LGPL-2.1-only |
| LAME (libmp3lame) | LGPL-2.0-or-later |
| libunistring | LGPL-3.0-or-later OR GPL-2.0-or-later |
| libevent, libvpx, Opus | BSD-3-Clause |
| dav1d | BSD-2-Clause |
| SVT-AV1 (libSvtAv1Enc) | BSD-3-Clause-Clear, plus the Alliance for Open Media patent license |
| libvmaf | BSD-2-Clause-Patent |
| json-c | MIT |
| libconfuse, libsodium | ISC |
| OpenSSL (libssl, libcrypto) | Apache-2.0 |
| SQLite | Public domain |
| XZ Utils (liblzma) | 0BSD |

Their sources are published by each project and mirrored by
[Homebrew](https://formulae.brew.sh). The sources of the GPL- and LGPL-licensed
ones are also attached to each release, as described above.

## App frameworks

| Component | License |
|---|---|
| [Sparkle](https://sparkle-project.org) 2.9.5 (update framework, currently disabled; license in `licenses/sparkle/`) | MIT |

## Fonts

| Component | License |
|---|---|
| [Poppins](https://github.com/itfoundry/Poppins) Medium, © 2020 The Poppins Project Authors (the wordmark typeface, in `Contents/Resources/Fonts/`; license in `licenses/poppins/OFL.txt`; also in this repository as `brand/fonts/`) | SIL Open Font License 1.1 |

The wordmark SVGs in `brand/` are outlined from Poppins Medium.

## Trademarks

AirPlay is a trademark of Apple Inc.; the AirPlay icon in the app is Apple's
SF Symbol, used as the system provides it. The Bluetooth® word mark and logos
are registered trademarks owned by Bluetooth SIG, Inc.; the app draws the
Bluetooth rune in code (`BluetoothRune` in `Sources/AudioSync/MenuBar.swift`)
only to mark speakers connected to this Mac over Bluetooth. AudioSync is not
affiliated with or endorsed by either company.
