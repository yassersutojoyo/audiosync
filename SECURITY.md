# Security

## Reporting a problem

Please report vulnerabilities privately through GitHub: Security → Report a vulnerability, or directly at <https://github.com/yassersutojoyo/audiosync/security/advisories/new>. Don't open a public issue for it. Steps to reproduce and the affected version or commit help a lot.

## How AudioSync is put together

AudioSync captures the audio source on your Mac and hands it to its bundled AirPlay engine (OwnTone), which runs as a separate child process and streams to your speakers. Most of the security story is about keeping that engine private to your Mac.

### The engine's API

Every time it starts the engine, AudioSync writes its config with `bind_address = "127.0.0.1"`, so the API only listens on this Mac. There are no trusted networks and a random `admin_password`, so every API request needs this run's password, which AudioSync sends as Basic auth. An AudioSync patch puts the same password on the HTTP audio stream and the library, so other user accounts, and local processes without the password, can't listen in. There's no `Access-Control-Allow-Origin`, so web pages in a browser can't read the API's responses, and without the password they can't control it. Websocket, MPD, RSP and local audio output are turned off, which means fewer open ports and less code running.

A few DAAP/DACP info endpoints (`/server-info`, `/content-codes`, `/ctrl-int`) still answer without a password; they only describe the server. If port 3689 is taken, the engine uses a free port instead.

The password sits in the config file (readable only by your user) just until the engine has loaded it, and then AudioSync rewrites the file without it. That keeps it off the disk, but it doesn't protect against software already running as your user, which macOS doesn't separate from AudioSync anyway.

### What faces the network

Only the AirPlay sockets listen on all interfaces: UDP control and timing, and PTP on 319/320. Speakers have to be able to reach those. The engine browses for AirPlay speakers (and `_touch-remote._tcp` for Remote pairing), which is what the Local Network permission is for. It doesn't advertise a library, web or media service. The only Bonjour records it publishes are the two OwnTone always registers for AirPlay remote control (`_dacp._tcp` "iTunes_Ctrl_…" and `_touch-able._tcp`, named by a hash), and both point at the loopback-only API, so other devices can't connect to them.

AudioSync sends no telemetry. Its network traffic is the localhost engine API, AirPlay discovery (mDNS browsing plus those two records) and streaming, and album artwork for Spotify's Now Playing, fetched from the https URL Spotify provides.

### Pairing

`setup.sh` patches OwnTone to try transient (PIN-less) pairing with HomePods. Who can stream is then decided by the HomePod's Allow Speaker & TV Access setting (Home app → Home Settings → Speakers & TV). Use "Anyone on the Same Network" on a network you trust, and avoid "Everyone".

A PIN or speaker password you type into AudioSync's prompt only goes to the engine's loopback API, which pairs with the speaker. AudioSync doesn't store or log it. The engine keeps only the resulting pairing key, in its database under `~/Library/Application Support/AudioSync/AirPlay`.

### Audio

Capture uses a Core Audio process tap (behind the Screen & System Audio Recording permission) or an input device (behind the Microphone permission). The audio stays in memory and goes to the local outputs and the engine's pipe in real time. It's never written to disk.

### Supply chain

`setup.sh` pins OwnTone and every library the engine links (versions and SHA-256 in `scripts/engine_deps.sh`), checks each download before building it from source, and stops if any of AudioSync's patches fails to apply. Homebrew only supplies build tools (`pkg-config`, `cmake`); none of its libraries end up in the app. `build.sh` pins Sparkle 2.9.5 by SHA-256, unless you pass an already verified copy with `SPARKLE_DIR`, which skips the check. The exact library versions are recorded in the app (`Contents/Resources/ThirdParty/versions.txt`).

## Signing and distribution

Releases are signed with a self-signed identity that `make-local-cert.sh` keeps in its own keychain, with the keychain password in a file only your user can read. They aren't notarized and don't use the Hardened Runtime (`build.sh` turns that on only for Developer ID identities). So Gatekeeper asks you to approve the first launch, and the signature only shows that later releases come from the same key, not who made them. Check downloads against `SHA256SUMS.txt` in the release.

On a contributor's Mac that key only signs local builds. On the Mac that builds releases it signs the public releases too, so anyone who can read that home folder could sign code that macOS treats as AudioSync for privacy permissions on every user's Mac. I keep a private backup of the keychain and its `signing-keychain.pass` file (in the AudioSync Application Support folder). Losing them would mean a new identity, and everyone would have to grant the privacy permissions again.

Developer ID signing with the Hardened Runtime and notarization is a possible later step; see [`RELEASE.md`](RELEASE.md).
