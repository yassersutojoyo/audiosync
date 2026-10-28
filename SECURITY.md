# Security

## Reporting a problem

Please report vulnerabilities privately through GitHub: Security → Report a vulnerability, or directly at <https://github.com/yassersutojoyo/audiosync/security/advisories/new>. Don't open a public issue for it. Steps to reproduce and the affected version or commit help a lot.

## How AudioSync is put together

AudioSync captures the audio source on your Mac or Windows PC and streams it to your AirPlay speakers with its own AirPlay sender, which runs inside the app. It starts no other process.

### The native sender

The native sender doesn't run a server or an API of its own, and it publishes no Bonjour records. Its mDNS browser shares UDP port 5353 with macOS's own responder and only sends queries. It browses `_raop._tcp` and `_airplay._tcp` over mDNS, which is what the Local Network permission is for, and leaves this Mac's own AirPlay Receiver out of the list. For each speaker you turn on, it opens an RTSP connection to the speaker, plus two UDP sockets (control and timing) on random ports, bound to the address that connection uses. The speaker sends its timing and resend requests to those, and the audio and track info go out over the same session. When streaming stops, the sessions and their sockets close. With AirPlay 2 (Experimental) on, an AirPlay 2 session also binds UDP ports 319 and 320 for PTP timing, on every address. It reads only port 319, and drops anything there that doesn't come from a speaker it's streaming to.

### Network

AudioSync sends no telemetry. Its network traffic is AirPlay and Google Cast discovery (mDNS browsing), streaming to the speakers you turn on, the web remote when you turn it on, album artwork for Spotify's Now Playing, fetched from the https URL Spotify provides, and, in release builds of the Mac and Windows apps, the update check: each fetches `appcast.xml` from the latest GitHub release over https (Sparkle on the Mac, AudioSync's own updater on Windows), and the update itself when you install one. It sends nothing beyond those requests (no system profile). Builds you make yourself never check.

Google Cast speakers get the audio, and the cover art of what's playing, over plain HTTP on your local network. For each stream AudioSync opens a server on a port the system picks, under a random path, and only answers the addresses of the Cast speaker or group it's streaming to.

### Web remote

The web remote is off until you turn it on. Then it serves plain HTTP on port 7655 (unless you pick another) and only lets in private, link-local and loopback addresses, plus IPv6 devices on the same network as your computer. A phone pairs by entering the 6-digit code AudioSync shows; each code works once and expires after 10 minutes. A device that sends 5 wrong codes within a minute has to wait, after 10 it can't try again until the code changes, and 40 wrong codes in total while a code works lock pairing for everyone until then. A paired device that sends no request for 90 days is forgotten and has to pair again. The server takes at most 32 connections at once and 8 from one device, and keeps some free for devices that are already paired. AudioSync keeps at most 8 paired devices and stores only a SHA-256 hash of each device's token and the day it last sent a request. There's no TLS, so the pairing code and the session cookie cross the network unencrypted. Someone on the same network who can watch the traffic could take over that session, see what's playing and turn your speakers on or off or change their volume and delay. Keep the web remote to a home network you trust. When a pairing or device file can't be fully read, AudioSync keeps a copy beside it (on Windows `airplay-pairings.ignored-*.dat`, still DPAPI-sealed with the old pairing keys, and `remote-devices.ignored-*.json`, with device token hashes) until you delete it, and forgetting or unpairing a device doesn't clear those copies.

### Pairing

An Apple TV that asks for a PIN pairs once, when you turn it on. The PIN goes only to the sender and is never logged. The resulting credentials are kept per receiver in `~/Library/Application Support/AudioSync/Pairings/airplay-pairings.dat`, readable and writable only by your user. That file isn't encrypted, so AudioSync keeps the `Pairings` folder (and only that folder) out of Time Machine backups. If part of the file can't be read, AudioSync copies it to `airplay-pairings.ignored-<seconds>.dat` in the same folder, readable only by your user, before it writes a new one; that copy holds the same keys, and Forget Pairing (shown while the Apple TV is paired or AudioSync keeps its credentials) deletes every copy that holds it, also a copy that holds it after the main file no longer does. A speaker password is kept in memory for the current run only and never written to disk or logged. The sender wipes its own copies of a password or PIN from memory once it no longer needs them. The text field you type it into and the strings the app passes it to the sender in aren't wiped.

Who can stream to a HomePod is decided by its Allow Speaker & TV Access setting (Home app → Home Settings → Speakers & TV). Use "Anyone on the Same Network" on a network you trust, and avoid "Everyone".

Versions up to 0.4.1 ran a bundled AirPlay engine that kept its files, including speaker pairing keys, under `~/Library/Application Support/AudioSync/AirPlay`. AudioSync no longer reads or writes that folder; you can delete it.

### Audio

Capture uses a Core Audio process tap (behind the Screen & System Audio Recording permission) or an input device (behind the Microphone permission). The audio stays in memory and goes to the local outputs and the AirPlay sender in real time. It's never written to disk.

### Supply chain

The Rust core and the native sender build with `--locked`, so their crates are exactly the ones in the committed `Cargo.lock`, and `scripts/build_core.sh` records each one's version and license in the app (`Contents/Resources/ThirdParty/licenses/rust/crates.txt`).

## Signing and distribution

Releases are signed with a self-signed identity that `make-local-cert.sh` keeps in its own keychain, with the keychain password in a file only your user can read. They aren't notarized and don't use the Hardened Runtime (`build.sh` turns that on only for Developer ID identities). So Gatekeeper asks you to approve the first launch, and the signature only shows that later releases come from the same key, not who made them. Check downloads against `SHA256SUMS.txt` in the release.

On a contributor's Mac that key only signs local builds. On the Mac that builds releases it signs the public releases too, so anyone who can read that home folder could sign code that macOS treats as AudioSync for privacy permissions on every user's Mac. I keep a private backup of the keychain and its `signing-keychain.pass` file (in the AudioSync Application Support folder). Losing them would mean a new identity, and everyone would have to grant the privacy permissions again.

Developer ID signing with the Hardened Runtime and notarization is a possible later step; see [`RELEASE.md`](RELEASE.md).
