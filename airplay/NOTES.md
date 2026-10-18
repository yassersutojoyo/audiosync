# audiosync-airplay implementation notes

Everything in this crate is written from `docs/AIRPLAY_SPEC.md`, whose clause IDs (`P1` and up) appear in comments and test names, from the RFCs it cites (2326 RTSP, 2617 digest authentication, 4566 SDP, 6762 and 6763 mDNS and DNS-SD, 5737 documentation addresses) and from the docs.rs pages of the crates below. `docs/AIRPLAY_SURVEY.md` was read to choose crates. No source code of any AirPlay sender or receiver was opened, and nothing else in this repository was used.

Where the spec leaves room, the readings below were chosen, each taking the clause's text literally where it has one.

## Discovery, RTSP and the AirPlay 1 session

### Layout

- `discovery.rs`: service types, device IDs, features and status flags, merged `Device`s with their `Verdict`, security modes, protocol path, codec and connect order; the `Directory` that merges browse events; the `Browser` trait a live mDNS browser implements later.
- `discovery/txt.rs`: TXT parsing and the RAOP and AirPlay keys the sender reads.
- `discovery/resolve.rs`: a `Resolver` that assembles services from raw PTR, SRV, TXT and address records, for a browser that hands those over.
- `rtsp.rs`: requests, responses, a parser for both directions, header helpers.
- `sdp.rs`, `digest.rs`, `volume.rs`, `metadata.rs`: the ANNOUNCE body, RFC 2617, the volume text and the DMAP, artwork and progress bodies.
- `session.rs`: the AirPlay 1 session from OPTIONS to TEARDOWN.
- `error.rs`, `ids.rs`: the error kinds of P133 and the random identifiers of P5.

### Design

The crate is sans-I/O. The session takes events in (`handle_connected`, `handle_input`, `handle_timeout`, `handle_connection_closed`, `handle_timing_request`, `handle_udp_send`) and host commands (`set_volume`, `read_volume`, `flush`, `send_metadata`, `teardown`), and gives `Output`s back: bytes to write, events, and the order to close. `poll_timeout` names the next timer. Time is `std::time::Instant`, passed in; random identifiers come from an `Entropy` source (`OsEntropy` uses `getrandom`), so tests are deterministic.

Seams for later stages:

- `discovery::Browser` and `discovery::Resolver`: a live mDNS browser.
- `session::Verifier`: the legacy pair-verify (P55, P56). The session already runs it in the right place and maps a non-200 answer to PairingRequired.
- `SessionConfig::auth_setup_key`: the public key of `POST /auth-setup` (P60); the pairing stage generates it.
- `SessionConfig::control_port` and `timing_port`: the transport stage opens both sockets before it creates the session, which is how P31's "open before SETUP" holds.
- `Session::media_allowed`, `handle_timing_request`, `handle_udp_send` and the `Recording` and `Flushed` events: what the RTP and timing stages read and feed.

### Spec readings

- **P5**: `<version>` in `User-Agent` is the crate's version, since the sender is the crate (Conventions).
- **P10**: a RAOP name without a valid 12-digit ID, or an AirPlay record without a valid `deviceid`, is listed as its own device, keyed by its instance. A device holds at most one service of each kind; a newer instance of the same kind replaces the older. The directory replaces a service's record on each resolution; addresses from several interfaces and families are merged by the resolver, which keys address records by host name.
- **P11**: when a key appears twice, only the first counts, and empty strings or strings that start with `=` are skipped (RFC 6763 §6.4). Values that aren't UTF-8 fail every typed accessor. AirPlay `pw` "present and not false" is read as a key without `=` or a value that parses as true; a malformed value drops the key, so it counts as absent.
- **P13**: `gid`, `igl`, `gcgl`, `acl`, `rsf`, `pi` and `psi` are kept as text, since the spec gives no format and the sender only reports them.
- **P14**: each `features` part is hex with or without `0x`; a third part, or a part wider than 32 bits, makes the value malformed. A device's features are AirPlay `features` when present, otherwise RAOP `ft`.
- **P15, P16**: RAOP `sf` and AirPlay `flags` are combined with OR. The AirPlay 1 conditions are checked in the clause's order (encryption, codec, transport, format) and the first failure is the reason. A device without `_raop._tcp` gets the AirPlay 1 reason "AirPlay 2 only", the only one of P139's reasons that fits.
- **P17**: every IPv6 address outside fe80::/10 counts as global. Duplicates are dropped and each group keeps the order it came in.
- **P18**: a record with TTL 0 is dropped at once (the one-second grace of RFC 6762 §10.1 changes nothing here). A service is resolved while it has a PTR, SRV and TXT record and at least one address for the SRV target; it is reported removed when any of them goes.
- **P2**: a RAOP record whose verdict is unsupported counts as an AirPlay 1 attempt that ended in Unsupported (P139 reports it from discovery). A Refused device that isn't AirPlay 2 capable stays on AirPlay 1.
- **P74**: the host's PCM opt-in wins over "ALAC when `cn` contains 1". A refused PCM ANNOUNCE comes back as Refused with `fallback: Some(Alac)`; the host opens a new session with ALAC, and `Directory::remember_pcm_refused` keeps the choice for the device. When both P74 and P46 could apply to a refused ANNOUNCE, the ALAC retry comes first, and the Apple TV rule then applies to the ALAC attempt.
- **P20**: the parser also accepts bare LF line ends and header continuation lines, as RFC 2326 §4 asks of readers. Limits: 64 KiB of head, 1 MiB of body.
- **P23, P28**: an IPv6 address goes into the URI without brackets and without a zone, the form of the one capture (G16); the SDP has no zone either.
- **P25**: `Session` goes on the methods the clause lists (RECORD, SET_PARAMETER, GET_PARAMETER, FLUSH, TEARDOWN), not on the keep-alive OPTIONS. A `;timeout=` parameter after the session ID (RFC 2326 §12.37) is not sent back.
- **P27**: method names compare case-sensitively (RFC 2326 §6.1).
- **P31**: a missing `timing_port` counts as 0. A SETUP answer without `Session` is a protocol error (a missing required header, P140).
- **P36 to P38**: a 401 without a usable Digest challenge (no realm or nonce, or an algorithm other than MD5) is a protocol error. Only `qop=auth` is used; a challenge that offers only `auth-int` is answered in the form without `qop`. One client nonce serves the session; `nc` counts the requests sent with the current nonce and starts again at 1 with a new one; `opaque` is echoed. The stale retry is allowed once per request, so a second `stale=true` answer to the same request means WrongPassword. A 401 to `POST /auth-setup` is not run through digest, since a failed auth-setup is not fatal (P60); a 401 to pair-verify is, before P58's non-200 rule.
- **P35, P129**: before SETUP has succeeded there is no `Session` to send, so stopping just closes the connection. With a request in flight, TEARDOWN waits for its answer; the 2 s count from the teardown call either way. An error during teardown ends it as Closed, not Failed.
- **P58**: "non-200" is literal: only 200 passes a verification step.
- **P116, P122, P126**: every `text/parameters` body ends in CRLF; P116 says so for volume, and the GET_PARAMETER and progress bodies follow the same line form (RFC 2326).
- **P117**: a level that rounds to zero is written `0.000000`, without a minus sign.
- **P118**: −144 given as a number is clamped to −30 like any other value; mute is its own value, `Volume::Muted`.
- **P120, P127**: "at most 10 per second" is at least 100 ms between two volume requests. Queue order while playing: FLUSH, volume, GET_PARAMETER, metadata, keep-alive. A pending volume holds back metadata and keep-alives.
- **P33, P126**: progress counts as metadata, so it carries `RTP-Info: rtptime=<current>`.
- **P124, P125**: the three DMAP items are always sent, empty strings included. Artwork is checked as JPEG by its `FF D8 FF` start.
- **P131**: the keep-alive OPTIONS is implemented. A failure status to it is Refused, as P138 says for OPTIONS.
- **P138**: 1xx and 3xx answers count as failures too: Refused for OPTIONS, ANNOUNCE, SETUP and RECORD, a warning for the others.
- **P140, P40**: a request line with a method the parser doesn't know is still a request from the receiver.
- **P141, P134**: a close reported while still connecting is Unreachable; after the connect it is Lost.
- **P133**: a failed session emits `Close`, then `Failed`.

### Dependencies and licenses

| Crate | Version | License | Why |
|---|---|---|---|
| md-5 | 0.11.0 | MIT OR Apache-2.0 | MD5 for digest authentication (RustCrypto) |
| getrandom | 0.4.3 | MIT OR Apache-2.0 | the OS random source for identifiers |
| digest, block-buffer, crypto-common, hybrid-array, typenum | 0.11.3, 0.12.1, 0.2.2, 0.4.15, 1.20.1 | MIT OR Apache-2.0 | md-5's dependencies |
| cfg-if, libc | 1.0.5, 0.2.191 | MIT OR Apache-2.0 | getrandom's dependencies (libc on Unix) |
| r-efi | 6.0.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later | getrandom on UEFI targets only, taken under MIT |

`deny.toml` mirrors core's allow-list (MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, Zlib, ISC, Unicode-3.0) with dev and build dependencies included; CI runs cargo-deny 0.20.2. The crate isn't bundled in either app yet, so `THIRD_PARTY_NOTICES.md` doesn't list it; it will when the crate ships.

### Spec coverage

`tests/spec_coverage.rs` reads the spec and fails unless every clause is named by a `// covers:` line or listed in `tests/spec_pending.txt`, never both. A clause counts as covered only when everything it asks of the sender is done; a test for part of a pending clause says "Part of P121" in a plain comment. After this stage: 144 clauses, 58 covered, 86 pending (audio and RTP, timing, pairing crypto, stream and transport, AirPlay 2). The test also checks that the IDs run P1 to Pn without gaps and that every clause names a source.
