# audiosync-airplay implementation notes

Everything in this crate is written from `docs/AIRPLAY_SPEC.md`, whose clause IDs (`P1` and up) appear in comments and test names, from the RFCs it cites (2326 RTSP, 2617 digest authentication, 3550 RTP, 3551 the audio profile, 4566 SDP, 5905 NTP timestamps, 6762 and 6763 mDNS and DNS-SD, 5737 and 3849 documentation addresses), from Apple's public description of the ALAC configuration record, from the author's own knowledge of the ALAC frame syntax for the uncompressed frame (see "Audio, RTP and timing"), and from the docs.rs and crates.io pages of the crates below. `docs/AIRPLAY_SURVEY.md` was read to choose crates. No source code of any AirPlay sender or receiver was opened, no ALAC codec source either, and nothing else in this repository was used.

Where the spec leaves room, the readings below were chosen, each taking the clause's text literally where it has one.

## Discovery, RTSP and the AirPlay 1 session

### Layout

- `discovery.rs`: service types, device IDs, features and status flags, merged `Device`s with their `Verdict`, security modes, protocol path, codec and connect order; the `Directory` that merges browse events; the `Browser` trait a live mDNS browser implements later.
- `discovery/txt.rs`: TXT parsing and the RAOP and AirPlay keys the sender reads.
- `discovery/resolve.rs`: a `Resolver` that assembles services from raw PTR, SRV, TXT and address records, for a browser that hands those over.
- `rtsp.rs`: requests, responses, a parser for both directions, header helpers.
- `sdp.rs`, `digest.rs`, `volume.rs`, `metadata.rs`: the ANNOUNCE body, RFC 2617, the volume text and the DMAP, artwork and progress bodies.
- `session.rs`: the AirPlay 1 session from OPTIONS to TEARDOWN, with the connect fallback of P17.
- `error.rs`, `ids.rs`: the error kinds of P133 and the random identifiers of P5 (the SSRC of P78 among them).

### Design

The crate is sans-I/O. The session takes events in (`handle_connected`, `handle_input`, `handle_timeout`, `handle_connection_closed`, `handle_timing_request`, `handle_udp_send`) and host commands (`set_volume`, `read_volume`, `flush`, `send_metadata`, `teardown`), and gives `Output`s back: bytes to write, events, and the order to close. `poll_timeout` names the next timer. Time is `std::time::Instant`, passed in; random identifiers come from an `Entropy` source (`OsEntropy` uses `getrandom`), so tests are deterministic.

Seams for later stages:

- `discovery::Browser` and `discovery::Resolver`: a live mDNS browser.
- `session::Verifier`: the legacy pair-verify (P55, P56). The session already runs it in the right place and maps a non-200 answer to PairingRequired.
- `SessionConfig::auth_setup_key`: the public key of `POST /auth-setup` (P60); the pairing stage generates it.
- `Output::Connect` and `Connection`: the transport stage connects to the address the session names, opens the control and timing sockets in that family, and hands their ports in with `handle_connected`, which is how P31's "open before SETUP" and P82's family hold.
- `Session::media_allowed`, `handle_timing_request`, `handle_udp_send`, `media_destinations` and the `Recording` and `Flushed` events: what the stream reads and feeds (next section).


### Spec readings

- **P5**: `<version>` in `User-Agent` is the crate's version, since the sender is the crate (Conventions).
- **P10**: a RAOP name without a valid 12-digit ID, or an AirPlay record without a valid `deviceid`, is listed as its own device, keyed by its instance. A device holds at most one service of each kind; a newer instance of the same kind replaces the older. The directory replaces a service's record on each resolution; addresses from several interfaces and families are merged by the resolver, which keys address records by host name.
- **P11**: when a key appears twice, only the first counts, and empty strings or strings that start with `=` are skipped (RFC 6763 §6.4). Values that aren't UTF-8 fail every typed accessor. AirPlay `pw` "present and not false" is read as a key without `=` or a value that parses as true; a malformed value drops the key, so it counts as absent.
- **P13**: `gid`, `igl`, `gcgl`, `acl`, `rsf`, `pi` and `psi` are kept as text, since the spec gives no format and the sender only reports them.
- **P14**: each `features` part is hex with or without `0x`; a third part, or a part wider than 32 bits, makes the value malformed. A device's features are AirPlay `features` when present, otherwise RAOP `ft`.
- **P15, P16**: RAOP `sf` and AirPlay `flags` are combined with OR. The AirPlay 1 conditions are checked in the clause's order (encryption, codec, transport, format) and the first failure is the reason. A device without `_raop._tcp` gets the AirPlay 1 reason "AirPlay 2 only", the only one of P139's reasons that fits.
- **P17**: every IPv6 address outside fe80::/10 counts as global. Duplicates are dropped and each group keeps the order it came in. A session takes the ordered addresses with the SRV port (`Device::connect_socket_addrs`) and asks for one at a time (`Output::Connect`); a refused attempt, or 5 s without a connect, moves to the next, and the session is Unreachable after the last (P134 counts its 5 s per attempt). The driver opens the control and timing sockets after the connect, in the connection's family (P82), and hands them in with the local address (`handle_connected`); the URI, the SDP and the UDP destinations (`MediaDestinations`, with the IPv6 scope) follow that connection.
- **P18**: a record with TTL 0 is dropped at once (the one-second grace of RFC 6762 §10.1 changes nothing here). A service is resolved while it has a PTR, SRV and TXT record and at least one address for the SRV target; it is reported removed when any of them goes.
- **P2**: a RAOP record whose verdict is unsupported counts as an AirPlay 1 attempt that ended in Unsupported (P139 reports it from discovery). A Refused device that isn't AirPlay 2 capable stays on AirPlay 1.
- **P74**: the host's PCM opt-in wins over "ALAC when `cn` contains 1". A refused PCM ANNOUNCE comes back as Refused with `fallback: Some(Alac)`; the host opens a new session with ALAC, and `Directory::remember_pcm_refused` keeps the choice for the device for as long as the directory lives, also while the device is gone from the network or not listed yet. When both P74 and P46 could apply to a refused ANNOUNCE, the ALAC retry comes first, and the Apple TV rule then applies to the ALAC attempt.
- **P20**: the parser also accepts bare LF line ends and header continuation lines, as RFC 2326 §4 asks of readers, skips empty lines before a start line, and reads a head that isn't UTF-8 with U+FFFD in place of the bad bytes (a stray byte in a header value is no protocol error). Limits: 64 KiB of head, 1 MiB of body.
- **P23, P28**: an IPv6 address goes into the URI without brackets and without a zone, the form of the one capture (G16); the SDP has no zone either.
- **P25**: `Session` goes on the methods the clause lists (RECORD, SET_PARAMETER, GET_PARAMETER, FLUSH, TEARDOWN), not on the keep-alive OPTIONS. A `;timeout=` parameter after the session ID (RFC 2326 §12.37) is not sent back.
- **P27**: method names compare case-sensitively (RFC 2326 §6.1).
- **P31**: a missing `timing_port` counts as 0. A SETUP answer without `Session` is a protocol error (a missing required header, P140).
- **P36 to P38**: a 401 without a usable Digest challenge (no realm or nonce, or an algorithm other than MD5) is a protocol error. Only `qop=auth` is used; a challenge whose `qop` lacks `auth` (only `auth-int`, or unknown options) can't be answered and is a protocol error that says so, rather than an answer without `qop` that would come back as a wrong password. Among several challenges the first answerable one is used.
 One client nonce serves the session; `nc` counts the requests sent with the current nonce and starts again at 1 with a new one; `opaque` is echoed. The stale retry is allowed once per request, so a second `stale=true` answer to the same request means WrongPassword. A 401 to `POST /auth-setup` is not run through digest, since a failed auth-setup is not fatal (P60); a 401 to pair-verify is, before P58's non-200 rule.
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

## Audio, RTP and timing

### Layout

- `audio.rs`: the stream format, float and 16-bit conversion, PCM payloads and the software volume gain (P72, P73, P76, P121).
- `alac.rs`: uncompressed ALAC frames and the configuration record that matches `a=fmtp:96` (P77, P29).
- `input.rs`: the host's `InputWriter` and the stream's reader: a preallocated single-producer, single-consumer ring of frames plus arrival records (P6, P111, P112).
- `clock.rs`: the sender clock and NTP-format timestamps (P89).
- `rtp.rs`: RTP headers, sync packets, retransmission requests and replies, timing requests and replies (P78, P86, P87, P90 to P93).
- `rate.rs`: the arrival model c(n) with R̂, and the send margin M (P84, P96, P101).
- `store.rs`: the payload stores keyed by packet number (P80, P88, P111).
- `stream.rs`: the `Stream` that cuts, encodes and paces packets for every speaker, with sync packets, resends, delay changes, joins, latency changes, restarts, latency reports and counters.

### Design

`Stream::new(config, clock, now)` gives the stream and the host's `InputWriter`; the host shares the `SenderClock` with its local outputs (P89). The driver adds a speaker with `add_speaker` once its session reports `Recording` (codec, `media_destinations`, SSRC and RECORD's numbering), removes it when the session stops playing, calls `handle_timeout` at `poll_timeout`, sends every `Transmit` from that session's control socket (`LocalSocket::Control`, P83), hands in control packets (`handle_control_packet`) and timing requests (`answer_timing`, whose reply leaves the timing socket for the request's source, and `Session::handle_timing_request` for P131), and turns every `StreamEvent::Flush` into `Session::flush`. `FirstSound` events and `speaker_latency` report when each speaker is heard; `Timeline` events report a new L or a restarted timeline, with the input position of frame 0.

Timeline packet j covers frames 352·j to 352·j + 351. It is cut from the input ring and encoded once, into a PCM store and an ALAC store, at c(352·j + 351) + M; each speaker sends it d_s later from the store with its own header. Speakers with `sv=true` or `sm=true` get their own encoding, cut at the same moment, in a store of their own. Stores are keyed by an absolute packet number that never repeats, so a restart doesn't confuse resends.

ALAC: P77 lets the encoder emit uncompressed frames, so the sender writes those (a channel-pair element with the escape flag set, the 352 sample pairs as they are, the end element; 1412 bytes). No encoder crate fits: `alac-encoder` 0.3.0 (Apache-2.0 OR MIT, a port of Apple's Apache-2.0 codec) was last released in January 2022, so it isn't maintained, and a port of Apache-2.0 code offered under MIT alone would be a licensing question of its own. Compression would only save bandwidth on a local network.

Where the frame layout comes from: P77 leaves the bitstream to Apple's codec, and no public document allowed here describes it (Apple's published ALAC description covers the configuration record only). The uncompressed layout (a 3-bit element tag, 1 for a channel pair; a 4-bit instance tag; 12 unused bits; a 1-bit flag for an explicit sample count; 2 bits of shifted low bytes; a 1-bit escape flag, set; the samples as they are, left and right interleaved, 16 bits each; the 3-bit end tag, 7; zero bits to the byte) was written from the author's own knowledge of the ALAC frame syntax. No ALAC source code and no bitstream document were opened for this work. The layout is then checked black-box: every test decodes the payloads with the independent `alac` decoder (a dev-dependency) set up from the `a=fmtp` line and gets the input back, bit for bit, so a wrong field would fail the build.

### Spec readings

- **P73**: "rounding to nearest" rounds halves away from zero; the product is formed in f64.
- **P75, P80, P111**: "one payload store" is one per codec: a PCM store, which is also the source of every own encoding, and an ALAC store, both allocated when the stream starts. Each holds 1024 + 376 + 14 packets: the resend history, the 3 s largest delay, and M's 100 ms plus one.
- **P77**: uncompressed frames only. The layout is the author's own reading of the ALAC frame syntax, confirmed against an independent decoder, not taken from Apple's codec source (see Design).
- **P81, P103**: a speaker's first packet is the first whose send time hasn't passed when it is added; k_s gives that packet the announced rtptime.
- **P84**: the residual is a block's arrival time minus c(n) of its last frame, so it is measured against the model that sets the send times; the host block is the largest write in the window. A late write (some frames dropped) still gives M its residual and block, so after one capture hiccup M covers the next ones, but it stays out of the rate fit; a late residual above M's 100 ms cap is a pause, not jitter, and is ignored.
 The cut and every send happen at their time, never early; "within 5 ms" is the driver's wake-up accuracy, and a packet sent more than 5 ms late is counted late (P144).
- **P85, P112, P96**: frames keep their ring positions and late ones are dropped, so a host that stops writing gets silence and one that falls briefly behind and catches up lands back in place. A write at least 1 s behind the first position not yet cut, or still behind while it arrives at real-time pace (at least half the previous late write's length after it), is a resumed capture, not a late one: a loopback that delivers nothing while the source is silent does that on every pause. It moves up to that first position, so the positions it skipped stay the silence the stream inserted (P96). The stream places its frames where they would be had they just arrived on schedule (the last one at the n whose c(n) is their arrival, never before the first frame not yet cut), forgets the rate fit's points from before the pause, and reports the new mapping in a `Timeline` event. The audio is back within about M and a packet of the resumed write.
- **P86, P90**: a packet counts as RTP when its first byte says version 2 (0x80 to 0xBF); the spec's 0x80 is the common case. Anything else on the control and timing sockets is ignored.
- **P87, P88**: a requested packet already resent three times is skipped like one no longer held, and counted with them.
- **P89, P91**: the clock is `Instant` from an origin the host picks. The driver reads the sender clock when a timing request arrives and right before the send call and passes both instants in; between them the crate only formats 32 bytes.
- **P93, P94**: the sync sequence field runs on across flushes. The 44 100-frame marks count from frame 0 of the current timeline, so a restart starts them again.
- **P96, P101**: at a timeline's start c(n) = t0 + (n + 1) / 44 100, as if frames had been arriving since t0. An update runs when the stream first needs c(n) at or past a sync point, with the arrivals seen by then. Its target is the least-squares slope over the last 30 s, once the points span 10 s; before that a fixed prior slope (nominal, or R̂ from before a resumed capture), since extrapolating with R̂'s own slope would feed its error back into itself. To that it adds the offset to the fitted line, spread over ten seconds and never more than 50 ppm of R̂: the offset is at most the host's jitter, which M covers, and steering it fast would make R̂, and the rate the sync packets imply, follow every jittered arrival. Then the change is limited as P101 says. Fit points are thinned to one per 8 ms.
- **P98**: a change of Lf flushes every speaker, each going on with its next packet, and keeps the timeline and the model; only P143 restarts the timeline. Values outside 11 025 to 88 200 are clamped.
- **P100**: during the 250 ms window the speaker plays on under its old delay. A speaker that hasn't sent anything yet just starts at its new schedule, without FLUSH, and a change back to the current delay cancels a pending flush.
- **P109**: a fact about receivers; the tests check the sender's side of it: a receiver that plays as the sync packets ask gets every packet about L − M before it plays it.
- **P110, P113**: `Audio-Latency` reaches the stream through `SpeakerConfig::audio_latency` and comes back in `speaker_latency`, never in Lf or a stated time.
- **P111**: the per-speaker parts (an own store, the 1024 resend headers) are allocated when the speaker is added; everything else when the stream starts. An own store is filled at once with the packets a delay can still reach (the last 376 + 14), so a delay raised soon after joining finds them.
- **P114**: the crate gives `local_output_delay(d_local)` = L + d_local, `migrate_local_delay` for the one-time migration, c(n) through `arrival_time`, and the frame index of host frames as the writer position minus the timeline's origin (signed: frame n is at position n + origin). Besides Lf changes and restarts, a resumed capture changes the origin, and the `Timeline` event reports it too.
- **P121**: the software volume applies when a packet is cut, so a delayed speaker hears a change d_s later. With `sm=true` alone the own encoding has gain 1 or 0 and starts unmuted; the session sends levels but never mute.
- **P143**: frames still in the input ring are dropped and counted; M keeps its value and forgets its history. A driver that wakes more than L after the next packet's time restarts the timeline the same way rather than send packets that would be heard after their play time.

- **P144**: a session's silent frames are the silence in the packets sent to it; input frames dropped is the stream's count, shown on every session's counters since they share the input.
- **P6**: one atomic word per frame keeps both sides in safe Rust; the writer publishes its position with release ordering after storing the frames.

## Dependencies and licenses

| Crate | Version | License | Why |
|---|---|---|---|
| md-5 | 0.11.0 | MIT OR Apache-2.0 | MD5 for digest authentication (RustCrypto) |
| getrandom | 0.4.3 | MIT OR Apache-2.0 | the OS random source for identifiers |
| digest, block-buffer, crypto-common, hybrid-array, typenum | 0.11.3, 0.12.1, 0.2.2, 0.4.15, 1.20.1 | MIT OR Apache-2.0 | md-5's dependencies |
| cfg-if, libc | 1.0.5, 0.2.191 | MIT OR Apache-2.0 | getrandom's dependencies (libc on Unix) |
| r-efi | 6.0.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later | getrandom on UEFI targets only, taken under MIT |
| alac | 0.5.0 | MIT/Apache-2.0 | development only: the reference ALAC decoder of the tests, without default features, so it brings no other crate |

`deny.toml` mirrors core's allow-list (MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, Zlib, ISC, Unicode-3.0) with dev and build dependencies included; CI runs cargo-deny 0.20.2. The crate isn't bundled in either app yet, so `THIRD_PARTY_NOTICES.md` doesn't list it; it will when the crate ships.

## Spec coverage

`tests/spec_coverage.rs` reads the spec and fails unless every clause is named by a `// covers:` line or listed in `tests/spec_pending.txt`, never both. A clause counts as covered only when everything it asks of the sender is done; a test for part of a pending clause says "Part of P6" in a plain comment. After the audio, RTP and timing stage: 144 clauses, 101 covered, 43 pending (stream and transport 8, pairing crypto 14, AirPlay 2 21).
 The test also checks that the IDs run P1 to Pn without gaps and that every clause names a source.
